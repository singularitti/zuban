use std::{borrow::Cow, cell::Cell, sync::Arc};
pub(super) use utils::AlreadySeen;

use parsa_python_cst::ParamKind;

use super::{
    super::{
        ArgumentIndexWithParam, FormatData, Generics, Match, Matcher, MismatchReason, OnTypeError,
        ResultContext, SignatureMatch,
    },
    ReplaceSelfInMatcher,
    type_var_matcher::TypeVarMatcher,
};
use crate::{
    arguments::{Arg, InferredArg},
    database::{Database, PointLink},
    debug,
    diagnostics::IssueKind,
    inference_state::InferenceState,
    inferred::Inferred,
    matching::{ErrorTypes, GotType, maybe_class_usage},
    node_ref::NodeRef,
    params::{
        InferrableParamIterator, Param, ParamArgument, WrappedParamType, WrappedStar,
        WrappedStarStar,
    },
    type_::{
        CallableContent, CallableParams, CallableWithParent, ClassGenerics, GenericItem,
        GenericsList, MaybeUnpackGatherer, NeverCause, ParamSpecTypeVars, ReplaceSelf,
        ReplaceTypeVarLikes, StringSlice, Tuple, TupleArgs, TupleUnpack, Type, TypeVarLikes,
        TypeVarManager, Variance, match_arbitrary_len_vs_unpack, match_unpack,
    },
    type_helpers::{Callable, Class, FuncLike, Function},
};

pub(crate) fn calc_callable_dunder_init_type_vars<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    class: &Class,
    mut callable: Callable<'a>,
    args: impl Iterator<Item = Arg<'db, 'a>>,
    add_issue: impl Fn(IssueKind),
    skip_first_param: bool,
    result_context: &mut ResultContext,
    on_type_error: Option<OnTypeError>,
) -> CalculatedTypeArgs {
    if let Some(c) = callable.defined_in.as_mut() {
        c.set_correct_generics_if_necessary_for_init_in_superclass()
    }
    calc_dunder_init_type_vars(i_s, class, &callable, |matcher, _| {
        calc_type_vars_for_callable_internal(
            i_s,
            matcher,
            &callable,
            Some(class),
            args,
            &add_issue,
            skip_first_param,
            class.node_ref.as_link(),
            result_context,
            on_type_error,
        )
    })
}

pub(crate) fn calc_class_dunder_init_type_vars<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    class: &'a Class,
    mut function: Function<'a, 'a>,
    args: impl Iterator<Item = Arg<'db, 'a>>,
    add_issue: impl Fn(IssueKind),
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
) -> CalculatedTypeArgs {
    if let Some(c) = function.class.as_mut() {
        c.set_correct_generics_if_necessary_for_init_in_superclass()
    }
    calc_dunder_init_type_vars(i_s, class, &function, |matcher, class_type_vars| {
        if class_type_vars.has_from_untyped_params() {
            calc_untyped_func_type_vars_with_matcher(
                matcher,
                i_s,
                &function,
                args,
                add_issue,
                true,
                class_type_vars,
                class.as_link(),
                result_context,
                on_type_error,
            )
        } else {
            calc_type_vars_for_func_internal(
                i_s,
                matcher,
                &function,
                Some(class),
                args,
                add_issue,
                true,
                class.node_ref.as_link(),
                result_context,
                Some(on_type_error),
            )
        }
    })
}

fn calc_dunder_init_type_vars<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    class: &'a Class,
    func_like: &dyn FuncLike,
    check: impl FnOnce(Matcher, &'a TypeVarLikes) -> CalculatedTypeArgs,
) -> CalculatedTypeArgs {
    debug!("Calculate __init__ type vars for class {}", class.name());
    let type_vars = class.type_vars(i_s);
    let class_matcher_needed =
        matches!(class.generics, Generics::NotDefinedYet { .. }) && !type_vars.is_empty();
    // Function type vars need to be calculated, so annotations are used.
    let func_type_vars = func_like.type_vars(i_s.db);

    let match_in_definition = class.node_ref.as_link();
    let mut tv_matchers = vec![];
    if class_matcher_needed {
        tv_matchers.push(TypeVarMatcher::new(match_in_definition, type_vars.clone()));
    }
    if !func_type_vars.is_empty() {
        tv_matchers.push(TypeVarMatcher::new(
            func_like.defined_at(),
            func_type_vars.clone(),
        ));
    }
    let as_self_type = || class.as_type(i_s.db);
    let matcher = Matcher::new(Some(class), func_like, tv_matchers, Some(&as_self_type));

    let mut type_arguments = check(matcher, type_vars);
    if !class_matcher_needed {
        type_arguments.type_arguments = match class.generics_as_list(i_s.db) {
            ClassGenerics::List(generics_list) => Some(generics_list),
            class_generics @ (ClassGenerics::ExpressionWithClassType(_)
            | ClassGenerics::SlicesWithClassTypes(_)) => Some(GenericsList::new_generics(
                Generics::from_class_generics(i_s.db, class.node_ref, &class_generics)
                    .iter(i_s.db)
                    .map(|g| g.into_generic_item())
                    .collect(),
            )),
            ClassGenerics::None => None,
            ClassGenerics::NotDefinedYet => unreachable!(),
        };
    }
    type_arguments
}

#[derive(Debug)]
pub(crate) struct CalculatedTypeArgs {
    pub(super) in_definition: PointLink,
    pub matches: SignatureMatch,
    pub(super) type_arguments: Option<GenericsList>,
    pub(super) type_var_likes: Option<TypeVarLikes>,
}

impl CalculatedTypeArgs {
    pub fn type_arguments_into_class_generics(self, db: &Database) -> ClassGenerics {
        match self.type_arguments_into_generics(db) {
            Some(generics) => ClassGenerics::List(generics),
            None => ClassGenerics::None,
        }
    }

    pub fn type_arguments_into_generics(mut self, db: &Database) -> Option<GenericsList> {
        if let Some(type_var_likes) = &self.type_var_likes
            && let Some(type_args) = self.type_arguments.take()
        {
            self.type_arguments = Some(if type_args.has_param_spec() {
                let mut type_args = type_args.into_vec();
                for type_arg in &mut type_args {
                    if let GenericItem::ParamSpecArg(param_spec_arg) = type_arg {
                        param_spec_arg.type_vars = Some(ParamSpecTypeVars {
                            type_vars: type_var_likes.clone(),
                            in_definition: self.in_definition,
                        });
                    }
                }
                GenericsList::generics_from_vec(type_args)
            } else {
                type_args.replace_type_var_likes(db, &mut |usage| {
                    let found = usage.as_type_var_like();
                    type_var_likes
                        .iter()
                        .any(|tvl| tvl == &found)
                        .then(|| found.as_never_generic_item(db, NeverCause::Inference))
                })
            })
        }
        self.type_arguments
    }

    pub fn into_return_type(
        self,
        i_s: &InferenceState,
        return_type: &Type,
        class: Option<&Class>,
        replace_self_type: ReplaceSelf,
    ) -> Inferred {
        if self.type_var_likes.is_some()
            && let Type::Class(c) = &return_type
        {
            let cls = c.class(i_s.db);
            if cls.is_protocol(i_s.db) {
                let members = &cls.use_cached_class_infos(i_s.db).protocol_members;
                if members.len() == 1
                    && NodeRef::new(cls.node_ref.file, members[0].name_index).as_code()
                        == "__call__"
                {
                    let had_error = Cell::new(false);
                    let inf = cls
                        .instance()
                        .type_lookup(i_s, |_| had_error.set(true), "__call__")
                        .into_inferred();
                    if !had_error.get() {
                        return self.into_return_type(
                            i_s,
                            &inf.as_cow_type(i_s),
                            None,
                            replace_self_type,
                        );
                    }
                }
            }
        }

        let mut type_ = return_type
            .replace_type_var_likes_and_self(
                i_s.db,
                &mut |usage| {
                    if let Some(c) = class
                        && let Some(generic_item) = maybe_class_usage(i_s.db, c, &usage)
                    {
                        return Some(generic_item);
                    }
                    if self.in_definition == usage.in_definition() {
                        return Some(self.type_arguments.as_ref().unwrap()[usage.index()].clone());
                    }
                    None
                },
                replace_self_type,
            )
            .unwrap_or_else(|| return_type.clone());
        if let Some(type_var_likes) = self.type_var_likes {
            fn create_callable_hierarchy(
                db: &Database,
                manager: &mut TypeVarManager<Arc<CallableContent>>,
                parent_callable: Option<Arc<CallableContent>>,
                type_var_likes: &TypeVarLikes,
                t: &Type,
            ) {
                t.find_in_type(db, &mut |t| {
                    if let Type::Callable(c) = t {
                        // TODO the is_callable_known is only known, because we recurse
                        // potentially multiple times into the same data structures, which is
                        // not really needed.
                        if !manager.is_callable_known(c) {
                            manager.register_callable(CallableWithParent {
                                defined_at: c.clone(),
                                parent_callable: parent_callable.clone(),
                            });
                            // The old type vars of the function are still relevant and should stay
                            // there!
                            for already_late_bound_tv in c.type_vars.iter() {
                                manager.add(already_late_bound_tv.clone(), Some(c.clone()));
                            }
                            // Try to add the new type vars if they match.
                            c.params.search_type_vars(&mut |u| {
                                let found = u.as_type_var_like();
                                if type_var_likes.iter().any(|tvl| tvl == &found) {
                                    manager.add(found, Some(c.clone()));
                                }
                            });
                            create_callable_hierarchy(
                                db,
                                manager,
                                Some(c.clone()),
                                type_var_likes,
                                t,
                            )
                        }
                    }
                    false
                });
            }

            let mut manager = TypeVarManager::default();
            create_callable_hierarchy(i_s.db, &mut manager, None, &type_var_likes, &type_);
            type_ = type_.rewrite_late_bound_callables(&manager);
            let mut unused_type_vars = vec![];
            for type_var_like in type_var_likes.iter() {
                if !manager.iter().any(|tvl| tvl == type_var_like) {
                    unused_type_vars.push(type_var_like)
                }
            }
            if !unused_type_vars.is_empty() {
                type_ = type_
                    .replace_type_var_likes(i_s.db, &mut |usage| {
                        (usage.in_definition() == self.in_definition).then(|| {
                            usage
                                .as_type_var_like()
                                .as_never_generic_item(i_s.db, NeverCause::Inference)
                        })
                    })
                    .unwrap_or(type_);
            }
            debug_assert_eq!(manager.into_type_vars().len(), 0);
        }
        if std::cfg!(debug_assertions) {
            type_.search_type_vars(&mut |usage| debug_assert!(usage.temporary_matcher_id() == 0));
        }
        Inferred::from_type(type_)
    }
}

pub(crate) fn calc_func_type_vars<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    function: Function<'a, 'a>,
    args: impl Iterator<Item = Arg<'db, 'a>>,
    add_issue: impl Fn(IssueKind),
    skip_first_param: bool,
    type_vars: &TypeVarLikes,
    match_in_definition: PointLink,
    replace_self: Option<ReplaceSelfInMatcher>,
    result_context: &mut ResultContext,
    on_type_error: Option<OnTypeError>,
) -> CalculatedTypeArgs {
    debug!("Calculate type vars for {}", function.diagnostic_string());
    calc_type_vars_for_func_internal(
        i_s,
        get_matcher(&function, match_in_definition, replace_self, type_vars),
        &function,
        None,
        args,
        add_issue,
        skip_first_param,
        match_in_definition,
        result_context,
        on_type_error,
    )
}

pub(crate) fn calc_untyped_func_type_vars<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    function: &Function<'a, 'a>,
    args: impl Iterator<Item = Arg<'db, 'a>>,
    add_issue: impl Fn(IssueKind),
    skip_first_param: bool,
    type_vars: &'a TypeVarLikes,
    match_in_definition: PointLink,
    replace_self: Option<ReplaceSelfInMatcher>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
) -> CalculatedTypeArgs {
    let matcher = get_matcher(function, function.as_link(), replace_self, type_vars);
    calc_untyped_func_type_vars_with_matcher(
        matcher,
        i_s,
        function,
        args,
        add_issue,
        skip_first_param,
        type_vars,
        match_in_definition,
        result_context,
        on_type_error,
    )
}

fn calc_untyped_func_type_vars_with_matcher<'db: 'a, 'a>(
    matcher: Matcher,
    i_s: &InferenceState<'db, '_>,
    function: &Function<'a, 'a>,
    args: impl Iterator<Item = Arg<'db, 'a>>,
    add_issue: impl Fn(IssueKind),
    skip_first_param: bool,
    type_vars: &'a TypeVarLikes,
    match_in_definition: PointLink,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
) -> CalculatedTypeArgs {
    calc_type_vars_with_callback(
        i_s,
        matcher,
        function,
        None,
        &add_issue,
        match_in_definition,
        result_context,
        Some(on_type_error),
        |matcher| {
            match_arguments_against_params(
                i_s,
                matcher,
                function,
                &add_issue,
                Some(on_type_error),
                InferrableParamIterator::new(
                    i_s.db,
                    function
                        .iter_untyped_params(match_in_definition, type_vars)
                        .skip(skip_first_param as usize),
                    args,
                ),
            )
        },
    )
}

pub(crate) fn calc_callable_type_vars<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    callable: Callable<'a>,
    args: impl Iterator<Item = Arg<'db, 'a>>,
    add_issue: impl Fn(IssueKind),
    skip_first_param: bool,
    result_context: &mut ResultContext,
    replace_self: Option<ReplaceSelf>,
    on_type_error: Option<OnTypeError>,
) -> CalculatedTypeArgs {
    let type_vars = &callable.content.type_vars;
    let x: &dyn Fn() -> Type = &|| replace_self.unwrap()().unwrap_or(Type::Self_);
    calc_type_vars_for_callable_internal(
        i_s,
        get_matcher(
            &callable,
            callable.content.defined_at,
            replace_self.map(|_| x),
            type_vars,
        ),
        &callable,
        None,
        args,
        add_issue,
        skip_first_param,
        callable.content.defined_at,
        result_context,
        on_type_error,
    )
}

fn get_matcher<'a>(
    func_like: &'a dyn FuncLike,
    match_in_definition: PointLink,
    replace_self: Option<ReplaceSelfInMatcher<'a>>,
    type_vars: &TypeVarLikes,
) -> Matcher<'a> {
    let matcher = if type_vars.is_empty() {
        vec![]
    } else {
        vec![TypeVarMatcher::new(match_in_definition, type_vars.clone())]
    };
    Matcher::new(None, func_like, matcher, replace_self)
}

fn apply_result_context(
    i_s: &InferenceState,
    matcher: &mut Matcher,
    result_context: &mut ResultContext,
    return_class: Option<&Class>,
    func_like: &dyn FuncLike,
    on_reset_class_type_vars: impl FnOnce(&mut Matcher, &Class),
) {
    result_context.with_type_if_exists_and_replace_type_var_likes(i_s, |expected| {
        if let Some(return_class) = return_class {
            // This is kind of a special case. Since __init__ has no return annotation, we simply
            // check if the classes match and then push the generics there.
            let type_var_likes = return_class.type_vars(i_s);
            if !type_var_likes.is_empty()
                && matches!(return_class.generics, Generics::NotDefinedYet { .. })
            {
                if Class::with_self_generics(i_s.db, return_class.node_ref)
                    .as_type(i_s.db)
                    .is_sub_type_of(i_s, matcher, expected)
                    .bool()
                {
                    matcher.reset_invalid_bounds_of_context(i_s)
                } else {
                    // Here we reset all bounds, because it did not match.
                    for tv_matcher in &mut matcher.type_var_matchers {
                        for calc in tv_matcher.calculating_type_args.iter_mut() {
                            *calc = Default::default();
                        }
                    }
                    on_reset_class_type_vars(matcher, return_class)
                }
            }
        } else {
            let return_type = func_like.inferred_return_type(i_s);
            // Fill the type var arguments from context
            return_type.is_sub_type_of(i_s, matcher, expected);
            matcher.reset_invalid_bounds_of_context(i_s)
        }
        debug!(
            "Finished trying to infer context type arguments: [{}]",
            matcher.type_var_matchers[0].debug_format(i_s.db)
        );
    });
}

fn calc_type_vars_for_func_internal<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    matcher: Matcher,
    function: &Function<'a, '_>,
    return_class: Option<&Class>,
    args: impl Iterator<Item = Arg<'db, 'a>>,
    add_issue: impl Fn(IssueKind),
    skip_first_param: bool,
    match_in_definition: PointLink,
    result_context: &mut ResultContext,
    on_type_error: Option<OnTypeError>,
) -> CalculatedTypeArgs {
    calc_type_vars_with_callback(
        i_s,
        matcher,
        function,
        return_class,
        &add_issue,
        match_in_definition,
        result_context,
        on_type_error,
        |matcher| {
            match_arguments_against_params(
                i_s,
                matcher,
                function,
                &add_issue,
                on_type_error,
                function.iter_args_with_params(i_s.db, args, skip_first_param),
            )
        },
    )
}

fn calc_type_vars_for_callable_internal<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    matcher: Matcher,
    callable: &Callable<'a>,
    return_class: Option<&Class>,
    args: impl Iterator<Item = Arg<'db, 'a>>,
    add_issue: impl Fn(IssueKind),
    skip_first_param: bool,
    match_in_definition: PointLink,
    result_context: &mut ResultContext,
    on_type_error: Option<OnTypeError>,
) -> CalculatedTypeArgs {
    calc_type_vars_with_callback(
        i_s,
        matcher,
        callable,
        return_class,
        &add_issue,
        match_in_definition,
        result_context,
        on_type_error,
        |matcher| match &callable.content.params {
            CallableParams::Simple(params) => match_arguments_against_params(
                i_s,
                matcher,
                callable,
                &add_issue,
                on_type_error,
                InferrableParamIterator::new(
                    i_s.db,
                    params.iter().skip(skip_first_param as usize),
                    args,
                ),
            ),
            CallableParams::Any(_) | CallableParams::Never(_) => SignatureMatch::new_true(),
        },
    )
}

fn calc_type_vars_with_callback<'db: 'a, 'a>(
    i_s: &InferenceState<'db, '_>,
    mut matcher: Matcher,
    func_like: &dyn FuncLike,
    return_class: Option<&Class>,
    add_issue: impl Fn(IssueKind),
    match_in_definition: PointLink,
    result_context: &mut ResultContext,
    on_type_error: Option<OnTypeError>,
    check_params: impl FnOnce(&mut Matcher) -> SignatureMatch,
) -> CalculatedTypeArgs {
    let mut had_wrong_init_type_var = false;
    if matcher.has_type_var_matcher() {
        let mut add_init_generics = |matcher: &mut Matcher, return_class: &Class| {
            if let Some(t) = func_like.first_self_or_class_annotation(i_s)
                && let Some(func_class) = func_like.class()
            {
                // When an __init__ has a self annotation, it's a bit special, because it influences
                // the generics.
                let m = Class::with_self_generics(i_s.db, return_class.node_ref)
                    .as_type(i_s.db)
                    .is_sub_type_of(i_s, matcher, &t);
                for entry in &mut matcher
                    .type_var_matchers
                    .first_mut()
                    .unwrap()
                    .calculating_type_args
                {
                    entry
                        .type_
                        .avoid_type_vars_from_class_self_arguments(func_class);
                }
                if !m.bool() {
                    had_wrong_init_type_var = true;
                    if on_type_error.is_some() {
                        add_issue(IssueKind::ArgumentIssue(
                            "Invalid self type in __init__".into(),
                        ))
                    }
                }
                if cfg!(debug_assertions) {
                    let args = &matcher
                        .type_var_matchers
                        .first()
                        .unwrap()
                        .debug_format(i_s.db);
                    debug!("Added __init__ generics as [{args}]");
                }
            }
        };
        if let Some(return_class) = return_class {
            add_init_generics(&mut matcher, return_class)
        }
        apply_result_context(
            i_s,
            &mut matcher,
            result_context,
            return_class,
            func_like,
            add_init_generics,
        )
    }
    let matches = check_params(&mut matcher);
    let mut result = matcher.into_type_arguments(i_s, match_in_definition);
    if matches!(result.matches, SignatureMatch::False { .. }) {
        if on_type_error.is_some() {
            add_issue(IssueKind::ArgumentTypeIssue(
                "Incompatible callable argument with type vars".into(),
            ))
        }
        result.matches = SignatureMatch::False { similar: false };
    } else {
        result.matches = matches;
    }
    if had_wrong_init_type_var {
        result.matches = SignatureMatch::False { similar: false };
    }
    if cfg!(feature = "zuban_debug")
        && let Some(type_arguments) = &result.type_arguments
    {
        debug!(
            "Calculated type vars for {}: [{}]",
            func_like
                .diagnostic_string(i_s.db)
                .as_deref()
                .unwrap_or("function"),
            type_arguments.format(&FormatData::new_short(i_s.db)),
        );
    }
    result
}

pub(crate) fn match_arguments_against_params<
    'db: 'x,
    'x,
    P: Param<'x>,
    AI: Iterator<Item = Arg<'db, 'x>>,
>(
    i_s: &InferenceState<'db, '_>,
    matcher: &mut Matcher,
    func_like: &dyn FuncLike,
    add_issue: &impl Fn(IssueKind),
    on_type_error: Option<OnTypeError>,
    mut args_with_params: InferrableParamIterator<'db, 'x, impl Iterator<Item = P>, P, AI>,
) -> SignatureMatch {
    let diagnostic_string = |prefix: &str| {
        (on_type_error.unwrap().generate_diagnostic_string)(func_like, i_s.db)
            .map(|s| (prefix.to_owned() + &s).into())
    };
    let too_few_arguments = || {
        if on_type_error.is_some() {
            let s = diagnostic_string(" for ").unwrap_or_else(|| Box::from(""));
            add_issue(IssueKind::TooFewArguments(s));
        }
    };
    let should_generate_errors = on_type_error.is_some();
    let mut missing_params = vec![];
    let mut missing_unpacked_typed_dict_names: Option<Vec<(StringSlice, bool)>> = None;
    let mut argument_indices_with_any = vec![];
    let mut matches = Match::new_true();
    // lambdas are analyzed at the end to improve type inference.
    let mut delayed_params = vec![];
    let mut params_iterator = args_with_params.by_ref().enumerate();
    let add_keyword_argument_issue_maybe_multi_value =
        |arg: &Arg, name: &str, is_multi_value_issue| {
            let s = match is_multi_value_issue {
                true => format!(
                    "{} gets multiple values for keyword argument \"{name}\"",
                    diagnostic_string("").as_deref().unwrap_or("function"),
                ),
                false => {
                    if arg.is_from_star_star_args() {
                        format!(
                            "Extra argument \"{name}\" from **args{}",
                            diagnostic_string(" for ").as_deref().unwrap_or(""),
                        )
                    } else {
                        format!(
                            "Unexpected keyword argument \"{name}\"{}",
                            diagnostic_string(" for ").as_deref().unwrap_or(""),
                        )
                    }
                }
            };
            if i_s.db.project.settings.mypy_compatible {
                // Mypy adds these issues on top of the whole function call instead of the specific
                // keyword argument.
                add_issue(IssueKind::ArgumentIssue(s.into()));
            } else {
                arg.add_issue(i_s, IssueKind::ArgumentIssue(s.into()));
            }
        };
    let add_keyword_argument_issue = |arg: &Arg, name: &str| {
        add_keyword_argument_issue_maybe_multi_value(
            arg,
            name,
            func_like.has_keyword_param_with_name(i_s.db, name),
        )
    };
    while let Some(((i, p), was_delayed)) = params_iterator
        .next()
        .map(|x| (x, false))
        .or_else(|| delayed_params.pop().map(|x| (x, true)))
    {
        if matches!(p.argument, ParamArgument::None) && !p.param.has_default() {
            matches = Match::new_false();
            if should_generate_errors {
                missing_params.push(p.param);
            }
            debug!(
                "Arguments for {:?} missing",
                p.param
                    .name(i_s.db)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("#{i}"))
            );
            continue;
        } else if p.argument.is_lambda_argument() && !was_delayed {
            delayed_params.push((i, p));
            continue;
        }
        let mut match_arg = |arg: &Arg<'db, '_>, might_have_type_vars, expected: Cow<Type>| {
            let value = if might_have_type_vars && matcher.might_have_defined_type_vars() {
                arg.infer(&mut ResultContext::WithMatcher {
                    type_: &expected,
                    matcher,
                })
            } else {
                arg.infer(&mut ResultContext::new_known(&expected))
            };
            let value = match value {
                InferredArg::Inferred(value) => value,
                InferredArg::StarredWithUnpack(with_unpack) => {
                    let m = match_arbitrary_len_vs_unpack(
                        i_s,
                        matcher,
                        &expected,
                        &with_unpack,
                        Variance::Covariant,
                    );
                    if let Match::False { reason, .. } = &m
                        && let Some(on_type_error) = on_type_error
                    {
                        let got = GotType::Starred(Type::Tuple(Tuple::new(TupleArgs::WithUnpack(
                            with_unpack,
                        ))));
                        let error_types = ErrorTypes {
                            matcher: Some(matcher),
                            reason,
                            got,
                            expected: &expected,
                        };
                        (on_type_error.callback)(i_s, &diagnostic_string, arg, error_types)
                    }
                    matches &= m;
                    return;
                }
                InferredArg::ParamSpec { usage } => {
                    let n = usage.param_spec.name(i_s.db);
                    if !expected.is_any() {
                        arg.add_argument_issue(
                            i_s,
                            &match p.param.kind(i_s.db) {
                                ParamKind::StarStar => format!("\"**{n}.kwargs\""),
                                _ => format!("\"*{n}.args\""),
                            },
                            &expected.format_short(i_s.db),
                            &diagnostic_string,
                        );
                        matches &= Match::new_false();
                    }
                    return;
                }
            };
            let value_t = value.as_cow_type(i_s);
            if matches!(value_t.as_ref(), Type::FunctionOverload(_))
                && !was_delayed
                && expected.has_type_vars()
            {
                // Function overloads are special, since they allow generics to assume multiple
                // potential forms. To make infering easier, just check them in the end.
                delayed_params.push((i, p.clone()));
                return;
            }
            let m = expected.is_super_type_of(i_s, matcher, &value_t);
            if let Match::False { reason, .. } = &m {
                debug!(
                    "Mismatch between {:?} and {:?} -> {:?}",
                    value_t.format_short(i_s.db),
                    expected.format_short(i_s.db),
                    &matches
                );
                if let Some(on_type_error) = on_type_error {
                    match reason {
                        MismatchReason::ConstraintMismatch { expected, type_var } => {
                            arg.add_issue(
                                i_s,
                                IssueKind::InvalidTypeVarValue {
                                    type_var_name: Box::from(type_var.name(i_s.db)),
                                    of: diagnostic_string("").unwrap_or(Box::from("function")),
                                    actual: expected.format_short(i_s.db),
                                },
                            );
                        }
                        _ => {
                            let error_types = ErrorTypes {
                                matcher: Some(matcher),
                                reason,
                                got: GotType::from_arg(i_s, arg, &value_t),
                                expected: &expected,
                            };
                            (on_type_error.callback)(i_s, &diagnostic_string, arg, error_types)
                        }
                    };
                }
            }
            if expected.type_of_protocol_to_type_of_protocol_assignment(i_s, &value) {
                add_issue(IssueKind::OnlyConcreteClassAllowedWhereTypeExpected {
                    type_: expected.format_short(i_s.db),
                })
            }
            if matches!(m, Match::True { with_any: true }) {
                argument_indices_with_any.push(ArgumentIndexWithParam {
                    argument_index: arg.index,
                    type_: expected.into_owned(),
                })
            }
            matches &= m
        };
        match &p.argument {
            ParamArgument::Argument(argument) => {
                let specific = p.param.specific(i_s.db);
                let expected = match specific {
                    WrappedParamType::PositionalOnly(t)
                    | WrappedParamType::PositionalOrKeyword(t)
                    | WrappedParamType::KeywordOnly(t)
                    | WrappedParamType::Star(WrappedStar::ArbitraryLen(t))
                    | WrappedParamType::StarStar(WrappedStarStar::ValueType(t)) => match t {
                        Some(t) => t,
                        None => {
                            // Simply infer the type to make sure type checking is done on the
                            // argument if there is no annotation.
                            argument.infer(&mut ResultContext::Unknown);
                            continue;
                        }
                    },
                    WrappedParamType::StarStar(WrappedStarStar::UnpackTypedDict(td)) => {
                        let all_members = td.members(i_s.db);
                        for member in all_members.named.iter() {
                            if let Some(m) = &missing_unpacked_typed_dict_names
                                && !m.iter().any(|(n, _)| *n == member.name)
                            {
                                continue;
                            }
                            match_arg(
                                argument,
                                p.param.might_have_type_vars(),
                                Cow::Borrowed(&member.type_),
                            )
                        }
                        if let Some(extra_items) = &all_members.extra_items {
                            match_arg(
                                argument,
                                p.param.might_have_type_vars(),
                                Cow::Borrowed(&extra_items.t),
                            )
                        }
                        //missing_unpacked_typed_dict_names.take();
                        continue;
                    }
                    WrappedParamType::Star(WrappedStar::UnpackedTuple(_)) => unreachable!(),
                    WrappedParamType::Star(WrappedStar::ParamSpecArgs(_))
                    | WrappedParamType::StarStar(WrappedStarStar::ParamSpecKwargs(_)) => {
                        unreachable!()
                    }
                };
                match_arg(argument, p.param.might_have_type_vars(), expected)
            }
            ParamArgument::ParamSpecArgs(..) => {
                let ParamArgument::ParamSpecArgs(param_spec, args) = p.argument else {
                    unreachable!()
                };
                matches &= match matcher.match_param_spec_arguments(
                    i_s,
                    &param_spec,
                    args,
                    func_like,
                    add_issue,
                    on_type_error,
                    &diagnostic_string,
                ) {
                    SignatureMatch::True { .. } | SignatureMatch::TrueWithAny { .. } => {
                        Match::new_true()
                    }
                    SignatureMatch::False { similar } => Match::False {
                        similar,
                        reason: MismatchReason::None,
                    },
                }
            }
            ParamArgument::TupleUnpack(args) => {
                let WrappedParamType::Star(WrappedStar::UnpackedTuple(expected)) =
                    p.param.specific(i_s.db)
                else {
                    unreachable!()
                };

                let mut gatherer = MaybeUnpackGatherer::default();
                let context_args = matcher.replace_type_var_likes_for_nested_context_in_tuple_args(
                    i_s.db,
                    expected.args.clone(),
                );
                for (i, arg) in args.iter().enumerate() {
                    if arg.in_args_or_kwargs_and_arbitrary_len() {
                        let maybe_err = match arg.infer(&mut ResultContext::Unknown) {
                            InferredArg::Inferred(_) => {
                                gatherer.add_unpack(TupleUnpack::ArbitraryLen(
                                    arg.infer_inferrable(i_s, &mut ResultContext::Unknown)
                                        .as_type(i_s),
                                ))
                            }
                            InferredArg::StarredWithUnpack(with_unpack) => {
                                gatherer.add_with_unpack(with_unpack)
                            }
                            InferredArg::ParamSpec { .. } => unreachable!(),
                        };
                        if maybe_err.is_err() {
                            add_issue(IssueKind::ArgumentIssue(
                                "Passing multiple variadic unpacks in a call is not supported"
                                    .into(),
                            ));
                            return SignatureMatch::False { similar: false };
                        }
                    } else {
                        // The context might not be correct, because there might have been a star
                        // arg and we would therefore need negative indexing. But since this only
                        // affects the context it might not be that urgent to change it.
                        let context_t = match &context_args {
                            TupleArgs::ArbitraryLen(t) => Some(t.as_ref()),
                            TupleArgs::FixedLen(ts) => ts.get(i),
                            TupleArgs::WithUnpack(with_unpack) => with_unpack.before.get(i),
                        };
                        let mut result_context = match context_t {
                            Some(t) => ResultContext::new_known(t),
                            None => ResultContext::Unknown,
                        };
                        let inf = arg.infer_inferrable(i_s, &mut result_context);
                        let t = inf.as_type(i_s);
                        gatherer.add_type(t)
                    }
                }
                match &expected.args {
                    TupleArgs::WithUnpack(with_unpack) => {
                        let actual = gatherer.into_tuple_args();
                        let match_ = match_unpack(
                            i_s,
                            matcher,
                            with_unpack,
                            &actual,
                            Variance::Covariant,
                            Some(&|mut error_types: ErrorTypes, index: isize| {
                                let Some(on_type_error) = on_type_error else {
                                    return;
                                };
                                let argument = if index >= 0 {
                                    if args.is_empty() {
                                        too_few_arguments();
                                        return;
                                    }
                                    // I'm pretty sure there were good reasons at the time why we
                                    // have to limit ourselfs to the len. It would be nice if
                                    // somebody eventually investigated and wrote a better comment
                                    // here, but it is likely that there are some cases that are so
                                    // complicated that it's just not worth it to 100% get the
                                    // index correct.
                                    &args[(index as usize).min(args.len() - 1)]
                                } else {
                                    let mut index = index + args.len() as isize;
                                    if index < 0 {
                                        index = 0;
                                    }
                                    &args[index as usize]
                                };
                                if let Some(star_t) = argument.maybe_star_type(i_s) {
                                    error_types.got = GotType::Starred(star_t)
                                }
                                (on_type_error.callback)(
                                    i_s,
                                    &diagnostic_string,
                                    argument,
                                    error_types,
                                )
                            }),
                            Some(&too_few_arguments),
                        );
                        matches &= match_;
                    }
                    // I tried to figure out a way to make this reachable, but it seems like it
                    // isn't.
                    TupleArgs::ArbitraryLen(_) => unreachable!(""),
                    TupleArgs::FixedLen(_) => unreachable!(),
                }
            }
            ParamArgument::MatchedUnpackedTypedDictMember {
                argument,
                type_,
                name,
            } => {
                // Checking totality for **Unpack[<TypedDict>]
                if let Some(name) = name {
                    if let Some(m) = missing_unpacked_typed_dict_names.as_mut() {
                        if let Some(index) = m.iter().position(|(n, _)| n == name) {
                            m.swap_remove(index);
                        } else {
                            matches = Match::new_false();
                            add_keyword_argument_issue_maybe_multi_value(
                                argument,
                                name.as_str(i_s.db),
                                true,
                            );
                            continue;
                        }
                    } else {
                        let WrappedParamType::StarStar(WrappedStarStar::UnpackTypedDict(td)) =
                            p.param.specific(i_s.db)
                        else {
                            unreachable!();
                        };
                        // Just fill the dict with all names and then remove them gradually.
                        missing_unpacked_typed_dict_names = Some(
                            td.members(i_s.db)
                                .named
                                .iter()
                                .filter(|m| &m.name != name)
                                .map(|m| (m.name, m.required))
                                .collect(),
                        );
                    }
                }
                match_arg(argument, true, Cow::Borrowed(type_))
            }
            ParamArgument::None => (),
        }
    }
    let add_missing_kw_issue = |param_name| {
        let mut s = format!("Missing named argument {:?}", param_name);
        s += diagnostic_string(" for ").as_deref().unwrap_or("");
        add_issue(IssueKind::ArgumentIssue(s.into()));
    };
    if args_with_params.too_many_positional_arguments {
        matches = Match::new_false();
        let s = "Too many positional arguments";
        if should_generate_errors {
            let mut s = s.to_owned();
            s += diagnostic_string(" for ").as_deref().unwrap_or("");
            add_issue(IssueKind::ArgumentIssue(s.into()));
        } else {
            debug!("{s}");
        }
    } else if args_with_params.has_unused_arguments() {
        matches = Match::new_false();
        if should_generate_errors {
            let mut too_many = false;
            while let Some(arg) = args_with_params.next_arg() {
                if let Some(key) = arg.keyword_name(i_s.db) {
                    add_keyword_argument_issue(&arg, key)
                } else {
                    too_many = true;
                    break;
                }
            }
            if too_many {
                let s = diagnostic_string(" for ").unwrap_or_else(|| Box::from(""));
                add_issue(IssueKind::TooManyArguments(s));
            }
        } else {
            debug!("Too many arguments found");
        }
    } else if !args_with_params.unused_keyword_arguments.is_empty() {
        matches = Match::new_false();
        if should_generate_errors {
            for unused in &args_with_params.unused_keyword_arguments {
                if let Some(key) = unused.keyword_name(i_s.db) {
                    add_keyword_argument_issue(unused, key)
                } else {
                    unreachable!();
                }
            }
        }
    } else if should_generate_errors {
        let mut missing_positional = vec![];
        for param in &missing_params {
            let param_kind = param.kind(i_s.db);
            if let Some(param_name) = param
                .name(i_s.db)
                .filter(|_| param_kind != ParamKind::PositionalOnly)
            {
                if param_kind == ParamKind::KeywordOnly {
                    add_missing_kw_issue(param_name)
                } else {
                    missing_positional.push(format!("\"{param_name}\""));
                }
            } else {
                too_few_arguments();
                break;
            }
        }
        if let Some(mut s) = match &missing_positional[..] {
            [] => None,
            [param_name] => Some(format!(
                "Missing positional argument {} in call",
                param_name
            )),
            _ => Some(format!(
                "Missing positional arguments {} in call",
                missing_positional.join(", ")
            )),
        } {
            s += diagnostic_string(" to ").as_deref().unwrap_or("");
            add_issue(IssueKind::ArgumentIssue(s.into()));
        };
    }
    if let Some(missing_unpacked_typed_dict_names) = missing_unpacked_typed_dict_names {
        for (missing, required) in missing_unpacked_typed_dict_names {
            if required {
                matches = Match::new_false();
                if should_generate_errors {
                    add_missing_kw_issue(missing.as_str(i_s.db))
                } else {
                    debug!("Unpacked typed dict mismatch");
                }
            }
        }
    }
    if let Some(unused_td) = &args_with_params.unused_unpack_typed_dict.maybe_unchecked() {
        for missing in unused_td.iter_required_members(i_s.db) {
            matches = Match::new_false();
            if should_generate_errors {
                add_missing_kw_issue(missing.name.as_str(i_s.db))
            } else {
                debug!("Unpacked typed dict mismatch");
            }
        }
    }
    match matches {
        Match::True { with_any: false } => SignatureMatch::True {
            arbitrary_length_handled: args_with_params.had_arbitrary_length_handled(),
        },
        Match::True { with_any: true } => SignatureMatch::TrueWithAny {
            argument_indices: argument_indices_with_any.into(),
        },
        Match::False { similar, .. } => SignatureMatch::False { similar },
    }
}
