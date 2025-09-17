use std::{mem, sync::Arc};

use parsa_python_cst::{
    ArgsIterator, Argument as CSTArgument, ArgumentsDetails, Comprehension, Expression,
    NamedExpression, NodeIndex, Primary, PrimaryContent,
};

use crate::{
    InferenceState,
    database::{Database, PointsBackup},
    debug,
    diagnostics::IssueKind,
    file::PythonFile,
    getitem::SliceType,
    inference_state::Mode,
    inferred::Inferred,
    matching::{IteratorContent, Matcher, ResultContext, UnpackedArgument},
    node_ref::NodeRef,
    type_::{IterCause, ParamSpecUsage, StringSlice, Type, TypedDict, WithUnpack},
};

pub(crate) trait Args<'db>: std::fmt::Debug {
    // Returns an iterator of arguments, where args are returned before kw args.
    // This is not the case in the grammar, but here we want that.
    fn iter<'x>(&'x self, mode: Mode<'x>) -> ArgIterator<'db, 'x>;
    fn calculate_diagnostics_for_any_callable(&self);
    fn as_node_ref_internal(&self) -> Option<NodeRef<'_>>;
    fn in_file(&self) -> Option<&PythonFile> {
        Some(self.as_node_ref_internal()?.file)
    }
    fn add_issue(&self, i_s: &InferenceState, issue: IssueKind) {
        self.as_node_ref_internal()
            .expect("Otherwise add_issue should be implemented")
            .add_issue(i_s, issue)
    }
    fn starting_line(&self, db: &Database) -> String {
        let Some(node_ref) = self.as_node_ref_internal() else {
            return "<unkown line>".into();
        };
        node_ref.line_one_based(db).to_string()
    }
    fn points_backup(&self) -> Option<PointsBackup> {
        None
    }
    fn reset_points_from_backup(&self, _backup: &Option<PointsBackup>) {
        // This is a bit special, but we use this to reset the type cache of the expressions to
        // avoid overload context inference issues.
    }

    fn has_a_union_argument(&self, i_s: &InferenceState<'db, '_>) -> bool {
        for arg in self.iter(i_s.mode) {
            if let InferredArg::Inferred(inf) = arg.infer(&mut ResultContext::Unknown)
                && inf.is_union_like(i_s)
            {
                return true;
            }
        }
        false
    }

    fn maybe_two_positional_args(
        &self,
        i_s: &InferenceState<'db, '_>,
    ) -> Option<(NodeRef<'db>, NodeRef<'db>)> {
        let mut iterator = self.iter(i_s.mode);
        let first_arg = iterator.next()?;
        let ArgKind::Positional(PositionalArg {
            node_ref: node_ref1,
            ..
        }) = first_arg.kind
        else {
            return None;
        };
        let second_arg = iterator.next()?;
        let ArgKind::Positional(PositionalArg {
            node_ref: node_ref2,
            ..
        }) = second_arg.kind
        else {
            return None;
        };
        if iterator.next().is_some() {
            return None;
        }
        Some((
            node_ref1.to_db_lifetime(i_s.db),
            node_ref2.to_db_lifetime(i_s.db),
        ))
    }

    fn maybe_single_positional_arg(
        &self,
        i_s: &InferenceState<'db, '_>,
        context: &mut ResultContext,
    ) -> Option<Inferred> {
        let mut iterator = self.iter(i_s.mode);
        let first = iterator.next()?;
        if iterator.next().is_some() {
            return None;
        }
        first.maybe_positional_arg(i_s, context)
    }

    fn maybe_simple_args(&self) -> Option<&SimpleArgs<'_, '_>> {
        None
    }
}

#[derive(Debug)]
pub(crate) struct SimpleArgs<'db, 'a> {
    // The node id of the grammar node called primary, which is defined like
    // primary "(" [arguments | comprehension] ")"
    file: &'a PythonFile,
    primary_node_index: NodeIndex,
    pub details: ArgumentsDetails<'a>,
    i_s: InferenceState<'db, 'a>,
}

impl<'db: 'a, 'a> Args<'db> for SimpleArgs<'db, 'a> {
    fn iter<'x>(&'x self, mode: Mode<'x>) -> ArgIterator<'db, 'x> {
        ArgIterator::new(match self.details {
            ArgumentsDetails::Node(arguments) => ArgIteratorBase::Iterator {
                i_s: self.i_s.with_mode(mode),
                file: self.file,
                iterator: arguments.iter().enumerate(),
                kwargs_before_star_args: {
                    let mut iterator = arguments.iter();
                    if iterator.any(|arg| matches!(arg, CSTArgument::Keyword(_))) {
                        if iterator.any(|arg| matches!(arg, CSTArgument::Star(_))) {
                            Some(vec![])
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                },
                ignore_metaclass_keyword: false,
            },
            ArgumentsDetails::Comprehension(comprehension) => {
                ArgIteratorBase::Comprehension(self.i_s.with_mode(mode), self.file, comprehension)
            }
            ArgumentsDetails::None => ArgIteratorBase::Finished,
        })
    }

    fn calculate_diagnostics_for_any_callable(&self) {
        // Mypy does not generate errors for `<any func>(*1)`. It however type checks the
        // expression after `*`>. It is debatable if this makes sense, because especially in
        // untyped code it's possible that there's a None in there that might annoy users.
        if self.i_s.db.project.settings.mypy_compatible {
            let inference = self.file.inference(&self.i_s);
            match self.details {
                ArgumentsDetails::Node(arguments) => {
                    for arg in arguments.iter() {
                        match arg {
                            CSTArgument::Positional(named_expr) => {
                                inference.infer_named_expression(named_expr);
                            }
                            CSTArgument::Keyword(kwarg) => {
                                inference.infer_expression(kwarg.unpack().1);
                            }
                            CSTArgument::Star(s) => {
                                inference.infer_expression(s.expression());
                            }
                            CSTArgument::StarStar(ss) => {
                                inference.infer_expression(ss.expression());
                            }
                        }
                    }
                }
                ArgumentsDetails::Comprehension(comp) => {
                    inference.infer_generator_comprehension(comp, &mut ResultContext::Unknown);
                }
                ArgumentsDetails::None => (),
            }
        } else {
            for arg in self.iter(self.i_s.mode) {
                arg.infer(&mut ResultContext::Unknown);
            }
        }
    }

    fn as_node_ref_internal(&self) -> Option<NodeRef<'_>> {
        Some(NodeRef::new(self.file, self.primary_node_index))
    }

    fn points_backup(&self) -> Option<PointsBackup> {
        let from = NodeRef::new(self.file, self.primary_node_index);
        let end = if let Some(primary_target) = from.maybe_primary_target() {
            primary_target.expect_closing_bracket_index()
        } else {
            let primary = from.expect_primary();
            primary.expect_closing_bracket_index()
        };
        Some(self.file.points.backup(self.primary_node_index..end))
    }

    fn reset_points_from_backup(&self, backup: &Option<PointsBackup>) {
        self.file.points.reset_from_backup(backup.as_ref().unwrap());
    }

    fn maybe_simple_args(&self) -> Option<&SimpleArgs<'_, '_>> {
        Some(self)
    }
}

impl<'db: 'a, 'a> SimpleArgs<'db, 'a> {
    pub fn new(
        i_s: InferenceState<'db, 'a>,
        file: &'a PythonFile,
        primary_node_index: NodeIndex,
        details: ArgumentsDetails<'a>,
    ) -> Self {
        Self {
            file,
            primary_node_index,
            details,
            i_s,
        }
    }

    pub fn from_primary(
        i_s: InferenceState<'db, 'a>,
        file: &'a PythonFile,
        primary_node: Primary<'a>,
    ) -> Self {
        match primary_node.second() {
            PrimaryContent::Execution(details) => {
                Self::new(i_s, file, primary_node.index(), details)
            }
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct KnownArgs<'a> {
    inferred: &'a Inferred,
    node_ref: NodeRef<'a>,
}

impl<'db> Args<'db> for KnownArgs<'_> {
    fn iter<'x>(&'x self, _: Mode<'x>) -> ArgIterator<'db, 'x> {
        ArgIterator::new(ArgIteratorBase::Inferred {
            inferred: self.inferred,
            node_ref: self.node_ref,
        })
    }

    fn calculate_diagnostics_for_any_callable(&self) {}

    fn as_node_ref_internal(&self) -> Option<NodeRef<'_>> {
        Some(self.node_ref)
    }
}

impl<'a> KnownArgs<'a> {
    pub fn new(inferred: &'a Inferred, node_ref: NodeRef<'a>) -> Self {
        Self { inferred, node_ref }
    }
}

impl<'a> KnownArgsWithCustomAddIssue<'a> {
    pub(crate) fn new(inferred: &'a Inferred, add_issue: &'a dyn Fn(IssueKind)) -> Self {
        Self {
            inferred,
            add_issue: CustomAddIssue(add_issue),
        }
    }
}

#[derive(Debug)]
pub(crate) struct KnownArgsWithCustomAddIssue<'a> {
    inferred: &'a Inferred,
    add_issue: CustomAddIssue<'a>,
}

impl<'db> Args<'db> for KnownArgsWithCustomAddIssue<'_> {
    fn iter<'x>(&'x self, _: Mode<'x>) -> ArgIterator<'db, 'x> {
        ArgIterator::new(ArgIteratorBase::InferredWithCustomAddIssue {
            inferred: self.inferred,
            add_issue: self.add_issue,
        })
    }
    fn calculate_diagnostics_for_any_callable(&self) {}

    fn add_issue(&self, _: &InferenceState, issue: IssueKind) {
        self.add_issue.0(issue)
    }

    fn as_node_ref_internal(&self) -> Option<NodeRef<'_>> {
        None
    }
}

#[derive(Debug)]
pub(crate) struct CombinedArgs<'db, 'a> {
    args1: &'a dyn Args<'db>,
    args2: &'a dyn Args<'db>,
}

impl<'db> Args<'db> for CombinedArgs<'db, '_> {
    fn iter<'x>(&'x self, mode: Mode<'x>) -> ArgIterator<'db, 'x> {
        let mut iterator = self.args1.iter(mode);
        debug_assert!(iterator.next.is_none()); // For now this is not supported
        iterator.next = Some((mode, self.args2));
        iterator
    }

    fn calculate_diagnostics_for_any_callable(&self) {
        self.args1.calculate_diagnostics_for_any_callable();
        self.args2.calculate_diagnostics_for_any_callable();
    }

    fn as_node_ref_internal(&self) -> Option<NodeRef<'_>> {
        self.args2.as_node_ref_internal()
    }

    fn starting_line(&self, db: &Database) -> String {
        self.args2.starting_line(db)
    }

    fn add_issue(&self, i_s: &InferenceState, issue: IssueKind) {
        self.args2.add_issue(i_s, issue)
    }

    fn points_backup(&self) -> Option<PointsBackup> {
        let first = self.args1.points_backup();
        let second = self.args2.points_backup();
        debug_assert!(!(first.is_some() && second.is_some()));
        first.or(second)
    }

    fn reset_points_from_backup(&self, backup: &Option<PointsBackup>) {
        self.args1.reset_points_from_backup(backup);
        self.args2.reset_points_from_backup(backup);
    }
}

impl<'db, 'a> CombinedArgs<'db, 'a> {
    pub(crate) fn new(args1: &'a dyn Args<'db>, args2: &'a dyn Args<'db>) -> Self {
        Self { args1, args2 }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PositionalArg<'db, 'a> {
    i_s: InferenceState<'db, 'a>,
    pub position: usize, // The position as a 1-based index
    pub node_ref: NodeRef<'a>,
    pub named_expr: NamedExpression<'a>,
}

impl PositionalArg<'_, '_> {
    pub fn infer(&self, result_context: &mut ResultContext) -> Inferred {
        self.node_ref
            .file
            .inference(&self.i_s)
            .infer_named_expression_with_context(self.named_expr, result_context)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct KeywordArg<'db, 'a> {
    i_s: InferenceState<'db, 'a>,
    pub key: &'a str,
    pub node_ref: NodeRef<'a>,
    pub expression: Expression<'a>,
}

impl KeywordArg<'_, '_> {
    pub fn infer(&self, result_context: &mut ResultContext) -> Inferred {
        self.node_ref
            .file
            .inference(&self.i_s)
            .infer_expression_with_context(self.expression, result_context)
    }

    pub(crate) fn add_issue(&self, i_s: &InferenceState, issue: IssueKind) {
        self.node_ref.add_issue(i_s, issue)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ArgKind<'db, 'a> {
    // Can be used for classmethod class or self in bound methods
    Keyword(KeywordArg<'db, 'a>),
    Inferred {
        inferred: Inferred,
        position: usize, // The position as a 1-based index
        node_ref: NodeRef<'a>,
        in_args_or_kwargs_and_arbitrary_len: bool,
        is_keyword: Option<Option<StringSlice>>,
    },
    InferredWithCustomAddIssue {
        inferred: Inferred,
        position: usize, // The position as a 1-based index
        add_issue: CustomAddIssue<'a>,
    },
    Positional(PositionalArg<'db, 'a>),
    StarredWithUnpack {
        with_unpack: WithUnpack,
        node_ref: NodeRef<'a>,
        position: usize, // The position as a 1-based index
    },
    ParamSpec {
        usage: ParamSpecUsage,
        node_ref: NodeRef<'a>,
        kwargs_node_ref: Option<NodeRef<'a>>,
        position: usize,
    },
    Comprehension {
        i_s: InferenceState<'db, 'a>,
        file: &'a PythonFile,
        comprehension: Comprehension<'a>,
    },
    Overridden {
        original: &'a Arg<'db, 'a>,
        inferred: Inferred,
    },
}

impl<'db, 'a> ArgKind<'db, 'a> {
    fn new_positional_return(
        i_s: InferenceState<'db, 'a>,
        position: usize,
        file: &'a PythonFile,
        named_expr: NamedExpression<'a>,
    ) -> BaseArgReturn<'db, 'a> {
        BaseArgReturn::Arg(ArgKind::Positional(PositionalArg {
            i_s,
            position,
            named_expr,
            node_ref: NodeRef {
                file,
                node_index: named_expr.index(),
            },
        }))
    }

    fn new_keyword_return(
        i_s: InferenceState<'db, 'a>,
        file: &'a PythonFile,
        key: &'a str,
        node_index: NodeIndex,
        expression: Expression<'a>,
    ) -> BaseArgReturn<'db, 'a> {
        BaseArgReturn::Arg(ArgKind::Keyword(KeywordArg {
            i_s,
            key,
            node_ref: NodeRef { file, node_index },
            expression,
        }))
    }
}

pub(crate) enum InferredArg<'a> {
    Inferred(Inferred),
    StarredWithUnpack(WithUnpack),
    ParamSpec { usage: &'a ParamSpecUsage },
}

#[derive(Debug, Clone)]
pub(crate) struct Arg<'db, 'a> {
    pub kind: ArgKind<'db, 'a>,
    pub index: usize,
}

impl<'db> Arg<'db, '_> {
    pub fn in_args_or_kwargs_and_arbitrary_len(&self) -> bool {
        match &self.kind {
            ArgKind::Inferred {
                in_args_or_kwargs_and_arbitrary_len,
                ..
            } => *in_args_or_kwargs_and_arbitrary_len,
            ArgKind::StarredWithUnpack { .. } => true,
            _ => false,
        }
    }

    pub fn is_arbitrary_kwargs(&self) -> bool {
        matches!(
            &self.kind,
            ArgKind::Inferred {
                in_args_or_kwargs_and_arbitrary_len: true,
                is_keyword: Some(None),
                ..
            }
        )
    }

    pub fn infer_inferrable(
        &self,
        _func_i_s: &InferenceState<'db, '_>,
        result_context: &mut ResultContext,
    ) -> Inferred {
        match self.infer(result_context) {
            InferredArg::Inferred(inf) => inf,
            _ => unreachable!(),
        }
    }

    pub fn infer(&self, result_context: &mut ResultContext) -> InferredArg<'_> {
        InferredArg::Inferred(match &self.kind {
            ArgKind::Inferred { inferred, .. }
            | ArgKind::InferredWithCustomAddIssue { inferred, .. } => (*inferred).clone(),
            ArgKind::Positional(positional) => positional.infer(result_context),
            ArgKind::Keyword(kw) => kw.infer(result_context),
            ArgKind::Comprehension {
                file,
                comprehension,
                i_s,
            } => file
                .inference(i_s)
                .infer_generator_comprehension(*comprehension, result_context),
            ArgKind::ParamSpec { usage, .. } => return InferredArg::ParamSpec { usage },
            ArgKind::StarredWithUnpack { with_unpack, .. } => {
                return InferredArg::StarredWithUnpack(with_unpack.clone());
            }
            ArgKind::Overridden { inferred, .. } => inferred.clone(),
        })
    }

    fn as_node_ref(&self) -> Result<NodeRef<'_>, CustomAddIssue<'_>> {
        match &self.kind {
            ArgKind::Positional(PositionalArg { node_ref, .. })
            | ArgKind::Keyword(KeywordArg { node_ref, .. })
            | ArgKind::ParamSpec { node_ref, .. }
            | ArgKind::StarredWithUnpack { node_ref, .. }
            | ArgKind::Inferred { node_ref, .. } => Ok(*node_ref),
            ArgKind::Comprehension {
                file,
                comprehension,
                ..
            } => Ok(NodeRef::new(file, comprehension.index())),
            ArgKind::Overridden { original, .. } => original.as_node_ref(),
            ArgKind::InferredWithCustomAddIssue { add_issue, .. } => Err(*add_issue),
        }
    }

    pub(crate) fn add_argument_issue(
        &self,
        i_s: &InferenceState,
        got: &str,
        expected: &str,
        error_text: &dyn Fn(&str) -> Option<Box<str>>,
    ) {
        self.add_issue(
            i_s,
            IssueKind::ArgumentTypeIssue(
                format!(
                    "Argument {}{} has incompatible type {got}; expected \"{expected}\"",
                    self.human_readable_index(i_s.db),
                    error_text(" to ").as_deref().unwrap_or(""),
                )
                .into(),
            ),
        );
    }

    pub(crate) fn add_issue(&self, i_s: &InferenceState, issue: IssueKind) {
        match self.as_node_ref() {
            Ok(node_ref) => node_ref.add_issue(i_s, issue),
            Err(add_issue) => add_issue.0(issue),
        }
    }

    pub fn maybe_star_type(&self, i_s: &InferenceState) -> Option<Type> {
        let Ok(node_ref) = self.as_node_ref() else {
            return None;
        };
        node_ref.maybe_starred_expression().map(|starred| {
            node_ref
                .file
                .inference(i_s)
                .infer_expression(starred.expression())
                .as_type(i_s)
        })
    }

    pub fn maybe_star_star_type(&self, i_s: &InferenceState) -> Option<Type> {
        let Ok(node_ref) = self.as_node_ref() else {
            return None;
        };
        node_ref
            .maybe_double_starred_expression()
            .and_then(|star_star| {
                // If we have a defined kwargs name, that's from a TypedDict and
                // shouldn't be formatted.
                if matches!(
                    &self.kind,
                    ArgKind::Inferred {
                        is_keyword: Some(Some(_)),
                        ..
                    }
                ) {
                    None
                } else {
                    Some(
                        node_ref
                            .file
                            .inference(i_s)
                            .infer_expression(star_star.expression())
                            .as_type(i_s),
                    )
                }
            })
    }

    pub fn is_from_star_star_args(&self) -> bool {
        let Ok(node_ref) = self.as_node_ref() else {
            return false;
        };
        node_ref.maybe_double_starred_expression().is_some()
    }

    pub fn human_readable_index(&self, db: &Database) -> String {
        match &self.kind {
            ArgKind::Inferred {
                is_keyword: Some(Some(s)),
                ..
            } => format!("\"{}\"", s.as_str(db)),
            ArgKind::Positional(PositionalArg { position, .. })
            | ArgKind::Inferred { position, .. }
            | ArgKind::InferredWithCustomAddIssue { position, .. }
            | ArgKind::StarredWithUnpack { position, .. }
            | ArgKind::ParamSpec { position, .. } => {
                format!("{position}")
            }
            ArgKind::Comprehension { .. } => "0".to_owned(),
            ArgKind::Keyword(KeywordArg { key, .. }) => format!("\"{key}\""),
            ArgKind::Overridden { original, .. } => original.human_readable_index(db),
        }
    }

    pub fn is_keyword_argument(&self) -> bool {
        matches!(
            self.kind,
            ArgKind::Keyword { .. }
                | ArgKind::Inferred {
                    is_keyword: Some(_),
                    ..
                }
        )
    }

    pub fn keyword_name(&self, db: &'db Database) -> Option<&str> {
        match &self.kind {
            ArgKind::Keyword(kw) => Some(kw.key),
            ArgKind::Inferred {
                is_keyword: Some(Some(key)),
                ..
            } => Some(key.as_str(db)),
            _ => None,
        }
    }

    pub fn maybe_positional_arg(
        self,
        i_s: &InferenceState<'db, '_>,
        context: &mut ResultContext,
    ) -> Option<Inferred> {
        match self.kind {
            ArgKind::Positional { .. } | ArgKind::Comprehension { .. } => {
                Some(self.infer_inferrable(i_s, context))
            }
            ArgKind::Inferred {
                inferred,
                in_args_or_kwargs_and_arbitrary_len: false,
                is_keyword: None,
                ..
            }
            | ArgKind::InferredWithCustomAddIssue { inferred, .. } => Some(inferred),
            ArgKind::Overridden { original, inferred } => original
                .clone()
                .maybe_positional_arg(i_s, context)
                .map(|_| inferred),
            ArgKind::ParamSpec { .. }
            | ArgKind::StarredWithUnpack { .. }
            | ArgKind::Keyword(KeywordArg { .. })
            | ArgKind::Inferred { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
enum ArgIteratorBase<'db, 'a> {
    Iterator {
        i_s: InferenceState<'db, 'a>,
        file: &'a PythonFile,
        iterator: std::iter::Enumerate<ArgsIterator<'a>>,
        ignore_metaclass_keyword: bool,
        kwargs_before_star_args: Option<Vec<CSTArgument<'a>>>,
    },
    Comprehension(InferenceState<'db, 'a>, &'a PythonFile, Comprehension<'a>),
    Inferred {
        inferred: &'a Inferred,
        node_ref: NodeRef<'a>,
    },
    InferredWithCustomAddIssue {
        inferred: &'a Inferred,
        add_issue: CustomAddIssue<'a>,
    },
    SliceType(InferenceState<'db, 'a>, SliceType<'a>),
    Finished,
}

enum BaseArgReturn<'db, 'a> {
    ArgsKwargs(ArgsKwargsIterator<'a>),
    Arg(ArgKind<'db, 'a>),
}

impl<'db, 'a> ArgIteratorBase<'db, 'a> {
    fn expect_i_s(&mut self) -> &InferenceState<'db, 'a> {
        if let Self::Iterator { i_s, .. } = self {
            i_s
        } else {
            unreachable!()
        }
    }
    fn into_argument_types(self, in_i_s: &InferenceState) -> Vec<Box<str>> {
        match self {
            Self::Inferred { inferred, .. } | Self::InferredWithCustomAddIssue { inferred, .. } => {
                vec![inferred.as_cow_type(in_i_s).format_short(in_i_s.db)]
            }
            Self::Iterator {
                i_s,
                file,
                iterator,
                ..
            } => iterator
                .map(|(_, arg)| {
                    let mut prefix = "".to_owned();
                    let inference = file.inference(&i_s);
                    let inf = match arg {
                        CSTArgument::Positional(named_expr) => {
                            inference.infer_named_expression(named_expr)
                        }
                        CSTArgument::Keyword(kwarg) => {
                            let (name, expr) = kwarg.unpack();
                            prefix = format!("{}=", name.as_code());
                            inference.infer_expression(expr)
                        }
                        CSTArgument::Star(starred_expr) => {
                            "*".clone_into(&mut prefix);
                            inference.infer_expression(starred_expr.expression())
                        }
                        CSTArgument::StarStar(double_starred_expr) => {
                            "*".clone_into(&mut prefix);
                            inference.infer_expression(double_starred_expr.expression())
                        }
                    };
                    format!("{prefix}{}", inf.format_short(&i_s)).into()
                })
                .collect(),
            Self::Comprehension(i_s, file, comprehension) => {
                vec![
                    file.inference(&i_s)
                        .infer_generator_comprehension(comprehension, &mut ResultContext::Unknown)
                        .format_short(&i_s),
                ]
            }
            Self::Finished => vec![],
            Self::SliceType(i_s, slice_type) => {
                vec![slice_type.infer(&i_s).format_short(&i_s)]
            }
        }
    }
}

impl<'db: 'a, 'a> Iterator for ArgIteratorBase<'db, 'a> {
    type Item = BaseArgReturn<'db, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Inferred { .. } => {
                if let Self::Inferred { inferred, node_ref } = mem::replace(self, Self::Finished) {
                    Some(BaseArgReturn::Arg(ArgKind::Inferred {
                        inferred: inferred.clone(),
                        position: 1,
                        node_ref,
                        in_args_or_kwargs_and_arbitrary_len: false,
                        is_keyword: None,
                    }))
                } else {
                    unreachable!()
                }
            }
            Self::Iterator {
                i_s,
                file,
                iterator,
                kwargs_before_star_args,
                ignore_metaclass_keyword,
            } => {
                for (i, arg) in iterator.by_ref() {
                    match arg {
                        CSTArgument::Positional(named_expr) => {
                            return Some(ArgKind::new_positional_return(
                                *i_s,
                                i + 1,
                                file,
                                named_expr,
                            ));
                        }
                        CSTArgument::Keyword(kwarg) => {
                            let (name, expression) = kwarg.unpack();
                            if *ignore_metaclass_keyword && name.as_code() == "metaclass" {
                                continue;
                            }
                            if let Some(kwargs_before_star_args) = kwargs_before_star_args {
                                kwargs_before_star_args.push(arg);
                            } else {
                                return Some(ArgKind::new_keyword_return(
                                    *i_s,
                                    file,
                                    name.as_code(),
                                    kwarg.index(),
                                    expression,
                                ));
                            }
                        }
                        CSTArgument::Star(starred_expr) => {
                            let inference = file.inference(i_s);
                            let inf = inference.infer_expression(starred_expr.expression());
                            let node_ref = NodeRef::new(file, starred_expr.index());
                            return match inf.as_cow_type(i_s).as_ref() {
                                Type::ParamSpecArgs(u1) => {
                                    let kwargs_node_ref = iterator.next().and_then(|p| {
                                        let CSTArgument::StarStar(next) = p.1 else {
                                            return None;
                                        };
                                        let inf = inference.infer_expression(next.expression());
                                        let t = inf.as_cow_type(i_s);
                                        let Type::ParamSpecKwargs(u2) = t.as_ref() else {
                                            return None;
                                        };
                                        (u1 == u2).then(|| NodeRef::new(file, p.1.index()))
                                    });
                                    if kwargs_node_ref.is_none() {
                                        node_ref.add_issue(
                                            i_s,
                                            IssueKind::ParamSpecArgumentsNeedsBothStarAndStarStar {
                                                name: u1.param_spec.name(i_s.db).into(),
                                            },
                                        )
                                    }
                                    Some(BaseArgReturn::Arg(ArgKind::ParamSpec {
                                        usage: u1.clone(),
                                        node_ref: NodeRef::new(file, starred_expr.index()),
                                        kwargs_node_ref,
                                        position: i + 1,
                                    }))
                                }
                                _ => Some(BaseArgReturn::ArgsKwargs(ArgsKwargsIterator::Args {
                                    iterator: inf.iter(i_s, node_ref, IterCause::VariadicUnpack),
                                    node_ref,
                                    position: i + 1,
                                })),
                            };
                        }
                        CSTArgument::StarStar(double_starred_expr) => {
                            let inf = file
                                .inference(i_s)
                                .infer_expression(double_starred_expr.expression());
                            let type_ = inf.as_cow_type(i_s);
                            let node_ref = NodeRef::new(file, double_starred_expr.index());
                            if let Some(typed_dict) = type_.maybe_typed_dict(i_s.db) {
                                return Some(BaseArgReturn::ArgsKwargs(
                                    ArgsKwargsIterator::TypedDict {
                                        db: i_s.db,
                                        typed_dict,
                                        iterator_index: 0,
                                        node_ref,
                                        position: i + 1,
                                    },
                                ));
                            }
                            let unpacked = unpack_star_star(i_s, &type_);
                            let s = i_s.db.python_state.str_type();
                            let value = if let Some((key, value)) = unpacked {
                                if !key.is_simple_sub_type_of(i_s, &s).bool() {
                                    debug!("Keyword is type {}", key.format_short(i_s.db));
                                    node_ref.add_issue(
                                        i_s,
                                        IssueKind::ArgumentIssue(Box::from(
                                            "Keywords must be strings",
                                        )),
                                    );
                                }
                                value
                            } else {
                                node_ref.add_issue(
                                    i_s,
                                    IssueKind::ArgumentTypeIssue(
                                        format!(
                                            "Argument after ** must be a mapping, not \"{}\"",
                                            type_.format_short(i_s.db),
                                        )
                                        .into(),
                                    ),
                                );
                                Type::ERROR
                            };
                            return Some(BaseArgReturn::ArgsKwargs(ArgsKwargsIterator::Kwargs {
                                inferred_value: Inferred::from_type(value),
                                node_ref,
                                position: i + 1,
                            }));
                        }
                    }
                }
                if let Some(kwargs_before_star_args) = kwargs_before_star_args
                    && let Some(kwarg_before_star_args) = kwargs_before_star_args.pop()
                {
                    match kwarg_before_star_args {
                        CSTArgument::Keyword(kwarg) => {
                            let (name, expression) = kwarg.unpack();
                            return Some(ArgKind::new_keyword_return(
                                *i_s,
                                file,
                                name.as_code(),
                                kwarg.index(),
                                expression,
                            ));
                        }
                        _ => unreachable!(),
                    }
                }
                None
            }
            Self::Comprehension(..) => {
                if let Self::Comprehension(i_s, file, comprehension) =
                    mem::replace(self, Self::Finished)
                {
                    Some(BaseArgReturn::Arg(ArgKind::Comprehension {
                        i_s,
                        file,
                        comprehension,
                    }))
                } else {
                    unreachable!()
                }
            }
            Self::Finished => None,
            Self::SliceType(..) => {
                let Self::SliceType(i_s, slice_type) = mem::replace(self, Self::Finished) else {
                    unreachable!()
                };
                Some(BaseArgReturn::Arg(ArgKind::Inferred {
                    inferred: slice_type.infer(&i_s),
                    position: 1,
                    node_ref: slice_type.as_argument_node_ref(),
                    in_args_or_kwargs_and_arbitrary_len: false,
                    is_keyword: None,
                }))
            }
            Self::InferredWithCustomAddIssue { .. } => {
                if let Self::InferredWithCustomAddIssue {
                    inferred,
                    add_issue,
                } = mem::replace(self, Self::Finished)
                {
                    Some(BaseArgReturn::Arg(ArgKind::InferredWithCustomAddIssue {
                        inferred: inferred.clone(),
                        position: 1,
                        add_issue,
                    }))
                } else {
                    unreachable!()
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ArgIterator<'db, 'a> {
    current: ArgIteratorBase<'db, 'a>,
    args_kwargs_iterator: ArgsKwargsIterator<'a>,
    next: Option<(Mode<'a>, &'a dyn Args<'db>)>,
    counter: usize,
}

impl<'db, 'a> ArgIterator<'db, 'a> {
    fn new(current: ArgIteratorBase<'db, 'a>) -> Self {
        Self {
            current,
            next: None,
            args_kwargs_iterator: ArgsKwargsIterator::None,
            counter: 0,
        }
    }

    pub fn new_slice(slice_type: SliceType<'a>, i_s: InferenceState<'db, 'a>) -> Self {
        // If you think this can be removed and replaced with ArgIteratorBase::Inferred, please
        // think about the fact that this will remove any way of inferring overloads with slices
        // and contexts.
        Self {
            current: ArgIteratorBase::SliceType(i_s, slice_type),
            args_kwargs_iterator: ArgsKwargsIterator::None,
            next: None,
            counter: 0,
        }
    }

    pub fn into_argument_types(mut self, i_s: &InferenceState<'db, '_>) -> Box<[Box<str>]> {
        let mut result = vec![];
        loop {
            result.extend(self.current.into_argument_types(i_s));
            if let Some((mode, next)) = self.next {
                self = next.iter(mode);
            } else {
                break;
            }
        }
        result.into_boxed_slice()
    }
}

impl<'db, 'a> Iterator for ArgIterator<'db, 'a> {
    type Item = Arg<'db, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match std::mem::replace(&mut self.args_kwargs_iterator, ArgsKwargsIterator::None) {
            ArgsKwargsIterator::None => match self.current.next() {
                Some(BaseArgReturn::Arg(mut kind)) => {
                    let index = self.counter;
                    if let ArgKind::Inferred { position, .. }
                    | ArgKind::InferredWithCustomAddIssue { position, .. } = &mut kind
                    {
                        // This is a bit of a special case where 0 means that we're on a bound self
                        // argument. In that case we do not want to increase the counter, because
                        // the bound argument is not counted as an argument.
                        if *position != 0 {
                            self.counter += 1;
                        }
                        *position += index;
                    } else {
                        self.counter += 1;
                    }
                    Some(Arg {
                        kind,
                        index: self.counter,
                    })
                }
                Some(BaseArgReturn::ArgsKwargs(args_kwargs)) => {
                    self.args_kwargs_iterator = args_kwargs;
                    self.next()
                }
                None => {
                    self.next?;
                    if let Some((mode, next)) = self.next {
                        let old_counter = self.counter;
                        *self = next.iter(mode);
                        self.counter += old_counter;
                        self.next()
                    } else {
                        None
                    }
                }
            },
            ArgsKwargsIterator::Args {
                mut iterator,
                node_ref,
                position,
            } => match iterator.next_as_argument(self.current.expect_i_s()) {
                Some(UnpackedArgument::Normal {
                    inferred,
                    arbitrary_len,
                }) => {
                    let index = self.counter;
                    self.counter += 1;
                    if !arbitrary_len {
                        self.args_kwargs_iterator = ArgsKwargsIterator::Args {
                            iterator,
                            node_ref,
                            position,
                        };
                    }
                    Some(Arg {
                        kind: ArgKind::Inferred {
                            inferred,
                            position,
                            node_ref,
                            in_args_or_kwargs_and_arbitrary_len: arbitrary_len,
                            is_keyword: None,
                        },
                        index,
                    })
                }
                Some(UnpackedArgument::WithUnpack(with_unpack)) => {
                    self.args_kwargs_iterator = ArgsKwargsIterator::WithUnpack {
                        with_unpack,
                        before_iterator_index: 0,
                        node_ref,
                        position,
                    };
                    self.next()
                }
                None => self.next(),
            },
            ArgsKwargsIterator::Kwargs {
                inferred_value,
                node_ref,
                position,
            } => {
                let index = self.counter;
                self.counter += 1;
                Some(Arg {
                    kind: ArgKind::Inferred {
                        inferred: inferred_value,
                        position,
                        node_ref,
                        in_args_or_kwargs_and_arbitrary_len: true,
                        is_keyword: Some(None),
                    },
                    index,
                })
            }
            ArgsKwargsIterator::TypedDict {
                db,
                node_ref,
                position,
                typed_dict,
                iterator_index,
            } => {
                let index = self.counter;
                self.counter += 1;
                // TODO extra_items: use
                let Some((name, t)) = typed_dict
                    .members(db)
                    .named
                    .get(iterator_index)
                    .map(|member| (member.name, member.type_.clone()))
                else {
                    return self.next();
                };
                self.args_kwargs_iterator = ArgsKwargsIterator::TypedDict {
                    db,
                    node_ref,
                    position,
                    typed_dict,
                    iterator_index: iterator_index + 1,
                };
                Some(Arg {
                    kind: ArgKind::Inferred {
                        inferred: Inferred::from_type(t),
                        position,
                        node_ref,
                        in_args_or_kwargs_and_arbitrary_len: false,
                        is_keyword: Some(Some(name)),
                    },
                    index,
                })
            }
            ArgsKwargsIterator::WithUnpack {
                mut with_unpack,
                mut before_iterator_index,
                position,
                node_ref,
            } => {
                let index = self.counter;
                self.counter += 1;
                if let Some(t) = with_unpack.before.get(before_iterator_index) {
                    let current_t = t.clone();
                    before_iterator_index += 1;
                    self.args_kwargs_iterator = ArgsKwargsIterator::WithUnpack {
                        with_unpack,
                        before_iterator_index,
                        position,
                        node_ref,
                    };
                    Some(Arg {
                        kind: ArgKind::Inferred {
                            inferred: Inferred::from_type(current_t),
                            position,
                            node_ref,
                            in_args_or_kwargs_and_arbitrary_len: false,
                            is_keyword: None,
                        },
                        // counter was increased before
                        index,
                    })
                } else {
                    if !with_unpack.before.is_empty() {
                        with_unpack.before = Arc::new([]);
                    }
                    Some(Arg {
                        kind: ArgKind::StarredWithUnpack {
                            with_unpack,
                            position,
                            node_ref,
                        },
                        index,
                    })
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum ArgsKwargsIterator<'a> {
    Args {
        iterator: IteratorContent,
        position: usize,
        node_ref: NodeRef<'a>,
    },
    Kwargs {
        inferred_value: Inferred,
        position: usize,
        node_ref: NodeRef<'a>,
    },
    TypedDict {
        db: &'a Database,
        typed_dict: Arc<TypedDict>,
        iterator_index: usize,
        position: usize,
        node_ref: NodeRef<'a>,
    },
    WithUnpack {
        with_unpack: WithUnpack,
        before_iterator_index: usize,
        position: usize,
        node_ref: NodeRef<'a>,
    },
    None,
}

pub fn unpack_star_star(i_s: &InferenceState, t: &Type) -> Option<(Type, Type)> {
    let wanted_cls = i_s.db.python_state.supports_keys_and_get_item_class(i_s.db);
    let mut matcher = Matcher::new_class_matcher(i_s, wanted_cls);
    let matches = wanted_cls.check_protocol_match(i_s, &mut matcher, t).bool();
    matches.then(|| {
        let mut iter = matcher.into_type_arg_iterator(i_s.db, wanted_cls.type_vars(i_s));
        (iter.next().unwrap(), iter.next().unwrap())
    })
}

pub(crate) struct NoArgs<'a> {
    node_ref: NodeRef<'a>,
    add_issue: Option<&'a dyn Fn(IssueKind)>,
}

impl<'a> NoArgs<'a> {
    pub fn new(node_ref: NodeRef<'a>) -> Self {
        Self {
            node_ref,
            add_issue: None,
        }
    }
    pub fn new_with_custom_add_issue(
        node_ref: NodeRef<'a>,
        add_issue: &'a dyn Fn(IssueKind),
    ) -> Self {
        Self {
            node_ref,
            add_issue: Some(add_issue),
        }
    }
}

impl std::fmt::Debug for NoArgs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("NoArgs")
            .field("node_ref", &self.node_ref)
            .field("has_add_issue", &self.add_issue.is_some())
            .finish()
    }
}

impl<'db> Args<'db> for NoArgs<'_> {
    fn iter<'x>(&'x self, _: Mode<'x>) -> ArgIterator<'db, 'x> {
        ArgIterator::new(ArgIteratorBase::Finished)
    }

    fn calculate_diagnostics_for_any_callable(&self) {}

    fn as_node_ref_internal(&self) -> Option<NodeRef<'_>> {
        Some(self.node_ref)
    }
}

#[derive(Debug)]
pub(crate) struct InitSubclassArgs<'db, 'a>(pub SimpleArgs<'db, 'a>);

impl<'db: 'a, 'a> Args<'db> for InitSubclassArgs<'db, 'a> {
    fn iter<'x>(&'x self, mode: Mode<'x>) -> ArgIterator<'db, 'x> {
        let mut iterator = self.0.iter(mode);
        for arg in iterator.clone() {
            if !arg.is_keyword_argument() {
                iterator.next();
            }
        }
        if let ArgIteratorBase::Iterator {
            ignore_metaclass_keyword,
            ..
        } = &mut iterator.current
        {
            // 'metaclass' keyword is consumed by the rest of the type machinery,
            // and is never passed to __init_subclass__ implementations
            *ignore_metaclass_keyword = true
        }
        iterator
    }

    fn calculate_diagnostics_for_any_callable(&self) {
        self.0.calculate_diagnostics_for_any_callable()
    }

    fn as_node_ref_internal(&self) -> Option<NodeRef<'_>> {
        self.0.as_node_ref_internal()
    }

    fn points_backup(&self) -> Option<PointsBackup> {
        self.0.points_backup()
    }

    fn reset_points_from_backup(&self, backup: &Option<PointsBackup>) {
        self.0.reset_points_from_backup(backup)
    }

    fn maybe_simple_args(&self) -> Option<&SimpleArgs<'_, '_>> {
        None
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CustomAddIssue<'a>(&'a dyn Fn(IssueKind));

impl std::fmt::Debug for CustomAddIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("ArgumentsWithCustomAddIssue").finish()
    }
}
