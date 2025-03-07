//! Handles new expressions and call expressions.
use std::{borrow::Cow, collections::HashMap};

use fxhash::FxHashMap;
use itertools::Itertools;
use rnode::{Fold, FoldWith, NodeId, VisitMut, VisitMutWith, VisitWith};
use stc_ts_ast_rnode::{
    RArrayPat, RBindingIdent, RCallExpr, RCallee, RComputedPropName, RExpr, RExprOrSpread, RIdent, RInvalid, RLit, RMemberExpr,
    RMemberProp, RNewExpr, RObjectPat, RPat, RStr, RTaggedTpl, RTsAsExpr, RTsEntityName, RTsLit, RTsThisTypeOrIdent, RTsType,
    RTsTypeParamInstantiation, RTsTypeRef,
};
use stc_ts_env::MarkExt;
use stc_ts_errors::{
    debug::{dump_type_as_string, dump_type_map, force_dump_type_as_string, print_type},
    DebugExt, ErrorKind,
};
use stc_ts_file_analyzer_macros::extra_validator;
use stc_ts_generics::type_param::finder::TypeParamUsageFinder;
use stc_ts_type_ops::{generalization::prevent_generalize, is_str_lit_or_union, Fix};
use stc_ts_types::{
    type_id::SymbolId, Alias, Array, Class, ClassDef, ClassMember, ClassProperty, CommonTypeMetadata, Function, Id, IdCtx,
    IndexedAccessType, Instance, Interface, Intersection, Key, KeywordType, KeywordTypeMetadata, LitType, Ref, Symbol, ThisType, Union,
    UnionMetadata,
};
use stc_ts_utils::PatExt;
use stc_utils::{cache::Freeze, ext::TypeVecExt};
use swc_atoms::js_word;
use swc_common::{Span, Spanned, SyntaxContext, TypeEq, DUMMY_SP};
use swc_ecma_ast::TsKeywordTypeKind;
use tracing::{debug, info, warn};
use ty::TypeExt;

use crate::{
    analyzer::{
        assign::AssignOpts,
        expr::TypeOfMode,
        generic::InferTypeOpts,
        scope::ExpandOpts,
        types::NormalizeTypeOpts,
        util::{make_instance_type, ResultExt},
        Analyzer, Ctx, ScopeKind,
    },
    ty,
    ty::{
        CallSignature, ConstructorSignature, FnParam, Method, MethodSignature, Type, TypeElement, TypeOrSpread, TypeParam,
        TypeParamInstantiation,
    },
    validator,
    validator::ValidateWith,
    VResult,
};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CallOpts {
    pub disallow_invoking_implicit_constructors: bool,

    /// Optional properties cannot be called.
    ///
    /// See: for-of29.ts
    pub disallow_optional_object_property: bool,

    /// If false, private members are not allowed.
    pub allow_private_names: bool,

    /// Used to prevent infinite recursion.
    pub do_not_check_object: bool,
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, node: &RExprOrSpread) -> VResult<TypeOrSpread> {
        let span = node.span();
        Ok(TypeOrSpread {
            span,
            spread: node.spread,
            ty: box node.expr.validate_with_default(self)?,
        })
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, e: &RCallExpr, type_ann: Option<&Type>) -> VResult<Type> {
        self.record(e);

        let RCallExpr {
            span,
            ref callee,
            ref args,
            ref type_args,
            ..
        } = *e;

        let mut type_ann = self.expand_type_ann(span, type_ann)?;
        type_ann.make_clone_cheap();

        let callee = match callee {
            RCallee::Super(..) => {
                self.report_error_for_super_refs_without_supers(span, true);
                self.report_error_for_super_reference_in_compute_keys(span, true);

                if type_args.is_some() {
                    // super<T>() is invalid.
                    self.storage.report(ErrorKind::SuperCannotUseTypeArgs { span }.into())
                }

                self.validate_args(args).report(&mut self.storage);

                self.scope.mark_as_super_called();

                return Ok(Type::any(span, Default::default()));
            }
            RCallee::Expr(callee) => callee,
            RCallee::Import(..) => todo!("dynamic import"),
        };

        let is_callee_iife = is_fn_expr(callee);

        // TODO(kdy1): validate children

        self.with_child(ScopeKind::Call, Default::default(), |analyzer: &mut Analyzer| {
            analyzer.ctx.is_calling_iife = is_callee_iife;

            analyzer.extract_call_new_expr_member(
                span,
                ReevalMode::Call(e),
                callee,
                ExtractKind::Call,
                args,
                type_args.as_deref(),
                type_ann.as_deref(),
            )
        })
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, e: &RNewExpr, type_ann: Option<&Type>) -> VResult<Type> {
        self.record(e);

        let RNewExpr {
            span,
            ref callee,
            ref args,
            ref type_args,
            ..
        } = *e;

        let mut type_ann = self.expand_type_ann(span, type_ann)?;
        type_ann.make_clone_cheap();

        // TODO(kdy1): e.visit_children

        self.with_child(ScopeKind::Call, Default::default(), |analyzer: &mut Analyzer| {
            analyzer.extract_call_new_expr_member(
                span,
                ReevalMode::New(e),
                callee,
                ExtractKind::New,
                args.as_ref().map(|v| &**v).unwrap_or_else(|| &mut []),
                type_args.as_deref(),
                type_ann.as_deref(),
            )
        })
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, e: &RTaggedTpl) -> VResult<Type> {
        let span = e.span;

        let tpl_str_arg = {
            let span = span.with_ctxt(SyntaxContext::empty());
            RExprOrSpread {
                spread: None,
                expr: box RExpr::TsAs(RTsAsExpr {
                    node_id: NodeId::invalid(),
                    span,
                    expr: box RExpr::Invalid(RInvalid { span: DUMMY_SP }),
                    type_ann: box RTsType::TsTypeRef(RTsTypeRef {
                        node_id: NodeId::invalid(),
                        span,
                        type_name: RTsEntityName::Ident(RIdent::new("TemplateStringsArray".into(), span)),
                        type_params: None,
                    }),
                }),
            }
        };
        let mut args = vec![tpl_str_arg];

        args.extend(e.tpl.exprs.iter().cloned().map(|expr| RExprOrSpread { spread: None, expr }));

        self.with_child(ScopeKind::Call, Default::default(), |analyzer: &mut Analyzer| {
            analyzer.extract_call_new_expr_member(
                span,
                ReevalMode::NoReeval,
                &e.tag,
                ExtractKind::Call,
                args.as_ref(),
                e.type_params.as_deref(),
                Default::default(),
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExtractKind {
    New,
    Call,
}

impl Analyzer<'_, '_> {
    /// Calculates the return type of a new /call expression.
    ///
    /// This method check arguments
    #[cfg_attr(debug_assertions, tracing::instrument(skip_all))]
    fn extract_call_new_expr_member(
        &mut self,
        span: Span,
        expr: ReevalMode,
        callee: &RExpr,
        kind: ExtractKind,
        args: &[RExprOrSpread],
        type_args: Option<&RTsTypeParamInstantiation>,
        type_ann: Option<&Type>,
    ) -> VResult<Type> {
        debug_assert_eq!(self.scope.kind(), ScopeKind::Call);

        let marks = self.marks();

        debug!("extract_call_new_expr_member");

        let type_args = match type_args {
            Some(v) => {
                let mut type_args = v.validate_with(self)?;
                self.prevent_expansion(&mut type_args);
                type_args.make_clone_cheap();
                Some(type_args)
            }
            None => None,
        };

        if self.ctx.in_computed_prop_name {
            if let Some(type_args) = &type_args {
                let mut v = TypeParamUsageFinder::default();
                type_args.visit_with(&mut v);
                self.report_error_for_usage_of_type_param_of_declaring_class(&v.params, span);
            }
        }

        match *callee {
            RExpr::Ident(ref i) if i.sym == js_word!("require") => {
                let id = args
                    .iter()
                    .cloned()
                    .map(|arg| match arg {
                        RExprOrSpread { spread: None, expr } => match *expr {
                            RExpr::Lit(RLit::Str(RStr { span, value, .. })) => RIdent::new(value, span).into(),
                            _ => unimplemented!("dynamic import: require()"),
                        },
                        _ => unimplemented!("error reporting: spread element in require()"),
                    })
                    .next()
                    .unwrap();
                if let Some(dep) = self.find_imported_var(&id)? {
                    let dep = dep;
                    unimplemented!("dep: {:#?}", dep);
                }

                // if let Some(Type::Enum(ref e)) = self.scope.find_type(&i.into()) {
                //     return Ok(RTsType::TsTypeRef(RTsTypeRef {
                //         span,
                //         type_name: RTsEntityName::Ident(i.clone()),
                //         type_params: None,
                //     })
                //     .into());
                // }
                Err(ErrorKind::UndefinedSymbol {
                    sym: i.into(),
                    span: i.span(),
                })?
            }

            _ => {}
        }

        match *callee {
            RExpr::Ident(RIdent {
                sym: js_word!("Symbol"), ..
            }) => {
                if kind == ExtractKind::New {
                    self.storage.report(ErrorKind::CannotCallWithNewNonVoidFunction { span }.into())
                }

                // Symbol uses special type
                if !args.is_empty() {
                    unimplemented!("Error reporting for calling `Symbol` with arguments is not implemented yet")
                }

                return Ok(Type::Symbol(Symbol {
                    span,
                    id: SymbolId::generate(),
                    metadata: Default::default(),
                }));
            }

            // Use general callee validation.
            RExpr::Member(RMemberExpr {
                prop:
                    RMemberProp::Computed(RComputedPropName {
                        expr: box RExpr::Lit(RLit::Num(..)),
                        ..
                    }),
                ..
            }) => {}

            RExpr::Member(RMemberExpr { ref obj, ref prop, .. }) => {
                let prop = self.validate_key(
                    &match prop {
                        RMemberProp::Ident(i) => RExpr::Ident(i.clone()),
                        RMemberProp::Computed(c) => *c.expr.clone(),
                        RMemberProp::PrivateName(p) => RExpr::PrivateName(p.clone()),
                    },
                    matches!(prop, RMemberProp::Computed(..)),
                )?;

                // Validate object
                let mut obj_type = obj
                    .validate_with_default(self)
                    .unwrap_or_else(|err| {
                        self.storage.report(err);
                        Type::any(span, Default::default())
                    })
                    .generalize_lit();
                {
                    // Handle toString()

                    if prop == js_word!("toString") {
                        return Ok(Type::from(KeywordType {
                            span,
                            kind: TsKeywordTypeKind::TsStringKeyword,
                            metadata: Default::default(),
                        }));
                    }
                }

                // Handle member expression
                obj_type.make_clone_cheap();

                let obj_type = match *obj_type.normalize() {
                    Type::Keyword(KeywordType {
                        kind: TsKeywordTypeKind::TsNumberKeyword,
                        ..
                    }) => self
                        .env
                        .get_global_type(span, &js_word!("Number"))
                        .expect("Builtin type named 'Number' should exist"),
                    Type::Keyword(KeywordType {
                        kind: TsKeywordTypeKind::TsStringKeyword,
                        ..
                    }) => self
                        .env
                        .get_global_type(span, &js_word!("String"))
                        .expect("Builtin type named 'String' should exist"),
                    _ => obj_type,
                };

                let mut arg_types = self.validate_args(args)?;
                arg_types.make_clone_cheap();

                let spread_arg_types = self.spread_args(&arg_types).context("tried to handle spreads in arguments")?;

                return self
                    .call_property(
                        span,
                        kind,
                        expr,
                        &obj_type,
                        &obj_type,
                        &prop,
                        type_args.as_ref(),
                        args,
                        &arg_types,
                        &spread_arg_types,
                        type_ann,
                        Default::default(),
                    )
                    .map(|ty| ty.fixed());
            }
            _ => {}
        }

        let ctx = Ctx {
            preserve_ref: false,
            ignore_expand_prevention_for_all: false,
            ignore_expand_prevention_for_top: false,
            preserve_ret_ty: true,
            preserve_params: true,
            ..self.ctx
        };

        self.with_ctx(ctx).with(|analyzer: &mut Analyzer| {
            let ret_ty = match callee {
                RExpr::Ident(i) if kind == ExtractKind::New => {
                    let mut ty = Type::Ref(Ref {
                        span: i.span,
                        type_name: RTsEntityName::Ident(i.clone()),
                        type_args: Default::default(),
                        metadata: Default::default(),
                    });
                    // It's specified by user
                    analyzer.prevent_expansion(&mut ty);
                    match ty {
                        Type::Ref(r) => Some(r),
                        _ => unreachable!(),
                    }
                }
                _ => None,
            };

            let mut callee_ty = {
                let callee_ty = callee.validate_with_default(analyzer).unwrap_or_else(|err| {
                    analyzer.storage.report(err);
                    Type::any(
                        span,
                        KeywordTypeMetadata {
                            common: CommonTypeMetadata {
                                implicit: true,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    )
                });
                match callee_ty.normalize() {
                    Type::Keyword(KeywordType {
                        kind: TsKeywordTypeKind::TsAnyKeyword,
                        ..
                    }) if type_args.is_some() => {
                        // If it's implicit any, we should postpone this check.
                        if !analyzer.is_implicitly_typed(&callee_ty) {
                            analyzer.storage.report(ErrorKind::AnyTypeUsedAsCalleeWithTypeArgs { span }.into())
                        }
                    }
                    _ => {}
                }

                match callee_ty.normalize() {
                    Type::Union(u) => {
                        let types = u
                            .types
                            .iter()
                            .cloned()
                            .filter(|callee| !matches!(callee.normalize(), Type::Module(..) | Type::Namespace(..)))
                            .collect::<Vec<_>>();

                        match types.len() {
                            0 => Type::never(
                                u.span,
                                KeywordTypeMetadata {
                                    common: u.metadata.common,
                                    ..Default::default()
                                },
                            ),
                            1 => types.into_iter().next().unwrap(),
                            _ => Type::Union(Union { types, ..*u }),
                        }
                    }
                    _ => callee_ty,
                }
            };

            if let Some(type_args) = &type_args {
                let type_params = match callee_ty.normalize() {
                    Type::Function(f) => f.type_params.as_ref(),
                    _ => None,
                };
                if let Some(type_param_decl) = type_params {
                    let mut params = FxHashMap::default();

                    for (type_param, ty) in type_param_decl.params.iter().zip(type_args.params.iter()) {
                        params.insert(type_param.name.clone(), ty.clone().freezed());
                    }

                    callee_ty = analyzer.expand_type_params(&params, callee_ty, Default::default())?;
                }
            }

            let ctx = Ctx {
                preserve_params: true,
                ..analyzer.ctx
            };
            callee_ty = analyzer.with_ctx(ctx).expand(
                span,
                callee_ty,
                ExpandOpts {
                    full: true,
                    expand_union: false,
                    ..Default::default()
                },
            )?;

            callee_ty.make_clone_cheap();

            analyzer.apply_type_ann_from_callee(span, kind, args, &callee_ty)?;
            let mut arg_types = analyzer.validate_args(args)?;
            arg_types.make_clone_cheap();

            let spread_arg_types = analyzer.spread_args(&arg_types).context("tried to handle spreads in arguments")?;

            let expanded_ty = analyzer.extract(
                span,
                expr,
                &callee_ty,
                kind,
                args,
                &arg_types,
                &spread_arg_types,
                type_args.as_ref(),
                type_ann,
                Default::default(),
            )?;

            Ok(expanded_ty.fixed())
        })
    }

    /// TODO(kdy1): Use Cow for `obj_type`
    ///
    /// ## Parameters
    ///
    ///  - `expr`: Can be default if argument does not include an arrow
    ///    expression nor a function expression.
    #[cfg_attr(debug_assertions, tracing::instrument(skip_all))]
    pub(super) fn call_property(
        &mut self,
        span: Span,
        kind: ExtractKind,
        expr: ReevalMode,
        this: &Type,
        obj_type: &Type,
        prop: &Key,
        type_args: Option<&TypeParamInstantiation>,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_ann: Option<&Type>,
        opts: CallOpts,
    ) -> VResult<Type> {
        obj_type.assert_valid();

        let span = span.with_ctxt(SyntaxContext::empty());

        let old_this = self.scope.this.take();
        self.scope.this = Some(this.clone());

        let res = (|| {
            let obj_type = self
                .normalize(
                    Some(span),
                    Cow::Borrowed(obj_type),
                    NormalizeTypeOpts {
                        preserve_intersection: true,
                        preserve_global_this: true,
                        ..Default::default()
                    },
                )
                .context("failed to normalize for call_property")?
                .freezed()
                .into_owned();

            match obj_type.normalize() {
                Type::Keyword(KeywordType {
                    kind: TsKeywordTypeKind::TsAnyKeyword,
                    ..
                }) => {
                    return Ok(Type::any(span, Default::default()));
                }

                Type::This(..) => {
                    if self.ctx.in_computed_prop_name {
                        self.storage
                            .report(ErrorKind::CannotReferenceThisInComputedPropName { span }.into());
                        // Return any to prevent other errors
                        return Ok(Type::any(span, Default::default()));
                    }
                }

                Type::Array(obj) => {
                    let obj = Type::Ref(Ref {
                        span,
                        type_name: RTsEntityName::Ident(RIdent::new(
                            "Array".into(),
                            span.with_ctxt(self.marks().unresolved_mark().as_ctxt()),
                        )),
                        type_args: Some(box TypeParamInstantiation {
                            span,
                            params: vec![*obj.elem_type.clone()],
                        }),
                        metadata: Default::default(),
                    });
                    return self.call_property(
                        span,
                        kind,
                        expr,
                        this,
                        &obj,
                        prop,
                        type_args,
                        args,
                        arg_types,
                        spread_arg_types,
                        type_ann,
                        opts,
                    );
                }

                Type::Intersection(obj) => {
                    let types = obj
                        .types
                        .iter()
                        .map(|obj| {
                            self.call_property(
                                span,
                                kind,
                                expr,
                                this,
                                obj,
                                prop,
                                type_args,
                                args,
                                arg_types,
                                spread_arg_types,
                                type_ann,
                                opts,
                            )
                        })
                        .filter_map(Result::ok)
                        .collect_vec();

                    if types.is_empty() {
                        if kind == ExtractKind::Call {
                            return Err(ErrorKind::NoCallablePropertyWithName {
                                span,
                                obj: box obj_type.clone(),
                                key: box prop.clone(),
                            }
                            .into());
                        } else {
                            return Err(ErrorKind::NoSuchConstructor {
                                span,
                                key: box prop.clone(),
                            }
                            .into());
                        }
                    }

                    return Ok(Type::union(types));
                }

                Type::Interface(ref i) => {
                    // We check for body before parent to support overriding
                    let err = match self.call_property_of_type_elements(
                        kind,
                        expr,
                        span,
                        &obj_type,
                        &i.body,
                        prop,
                        type_args,
                        args,
                        arg_types,
                        spread_arg_types,
                        type_ann,
                        opts,
                    ) {
                        Ok(v) => return Ok(v),
                        Err(err) => err,
                    };

                    // Check parent interface
                    for parent in &i.extends {
                        let parent = self
                            .type_of_ts_entity_name(span, &parent.expr, parent.type_args.as_deref())
                            .context("tried to check parent interface to call a property of it")?;
                        if let Ok(v) = self.call_property(
                            span,
                            kind,
                            expr,
                            this,
                            &parent,
                            prop,
                            type_args,
                            args,
                            arg_types,
                            spread_arg_types,
                            type_ann,
                            opts,
                        ) {
                            return Ok(v);
                        }
                    }

                    return Err(err);
                }

                Type::TypeLit(ref t) => {
                    return self.call_property_of_type_elements(
                        kind,
                        expr,
                        span,
                        &obj_type,
                        &t.members,
                        prop,
                        type_args,
                        args,
                        arg_types,
                        spread_arg_types,
                        type_ann,
                        opts,
                    );
                }

                Type::ClassDef(cls) => {
                    if let Some(v) = self.call_property_of_class(
                        span,
                        expr,
                        kind,
                        this,
                        cls,
                        prop,
                        true,
                        type_args,
                        args,
                        arg_types,
                        spread_arg_types,
                        type_ann,
                        opts,
                    )? {
                        return Ok(v);
                    }
                }

                Type::Class(ty::Class { def, .. }) => {
                    if let Some(v) = self.call_property_of_class(
                        span,
                        expr,
                        kind,
                        this,
                        def,
                        prop,
                        false,
                        type_args,
                        args,
                        arg_types,
                        spread_arg_types,
                        type_ann,
                        opts,
                    )? {
                        return Ok(v);
                    }
                }

                Type::Keyword(KeywordType {
                    kind: TsKeywordTypeKind::TsSymbolKeyword,
                    ..
                }) => {
                    if let Ok(ty) = self.env.get_global_type(span, &js_word!("Symbol")) {
                        return Ok(ty);
                    }
                }

                _ => {}
            }

            // Handle methods from `Object`.
            match obj_type.normalize() {
                Type::Interface(Interface { name, .. }) if *name.sym() == js_word!("Object") => {}
                _ => {
                    if !opts.do_not_check_object {
                        let obj_res = self.call_property(
                            span,
                            kind,
                            expr,
                            this,
                            &Type::Ref(Ref {
                                span: DUMMY_SP,
                                type_name: RTsEntityName::Ident(RIdent::new(
                                    js_word!("Object"),
                                    DUMMY_SP.with_ctxt(self.marks().unresolved_mark().as_ctxt()),
                                )),
                                type_args: None,
                                metadata: Default::default(),
                            }),
                            prop,
                            type_args,
                            args,
                            arg_types,
                            spread_arg_types,
                            type_ann,
                            CallOpts {
                                do_not_check_object: true,
                                ..opts
                            },
                        );
                        if let Ok(v) = obj_res {
                            return Ok(v);
                        }
                    }
                }
            }

            // Use proper error.
            if let Type::Class(..) = obj_type.normalize() {
                return Err(match kind {
                    ExtractKind::Call => ErrorKind::NoCallablePropertyWithName {
                        span,
                        obj: box obj_type.clone(),
                        key: box prop.clone(),
                    }
                    .into(),
                    ExtractKind::New => ErrorKind::NoSuchConstructor {
                        span,
                        key: box prop.clone(),
                    }
                    .into(),
                });
            }

            let ctx = Ctx {
                disallow_unknown_object_property: true,
                ..self.ctx
            };
            let callee = self
                .with_ctx(ctx)
                .access_property(span, &obj_type, prop, TypeOfMode::RValue, IdCtx::Var, Default::default())
                .context("tried to access property to call it")?;

            let callee_before_expanding = force_dump_type_as_string(&callee);
            let callee = self
                .normalize(Some(span), Cow::Owned(callee), NormalizeTypeOpts { ..Default::default() })?
                .into_owned();

            if let Type::ClassDef(cls) = callee.normalize() {
                if cls.is_abstract {
                    self.storage.report(ErrorKind::CannotCreateInstanceOfAbstractClass { span }.into())
                }
            }
            let callee_str = force_dump_type_as_string(&callee);

            self.get_best_return_type(span, expr, callee, kind, type_args, args, arg_types, spread_arg_types, type_ann)
                .convert_err(|err| match err {
                    ErrorKind::NoCallSignature { span, .. } => ErrorKind::NoCallablePropertyWithName {
                        span,
                        obj: box obj_type.clone(),
                        key: box prop.clone(),
                    },
                    ErrorKind::NoNewSignature { span, .. } => ErrorKind::NoConstructablePropertyWithName {
                        span,
                        obj: box obj_type.clone(),
                        key: box prop.clone(),
                    },
                    _ => err,
                })
                .with_context(|| {
                    format!(
                        "tried to call property by using access_property because the object type is not handled by call_property: \nobj = \
                         {}\ncallee = {}\ncallee (before expanding): {}",
                        force_dump_type_as_string(&obj_type),
                        callee_str,
                        callee_before_expanding,
                    )
                })
        })()
        .with_context(|| format!("tried to call a property of an object ({})", dump_type_as_string(obj_type)));
        self.scope.this = old_this;
        res
    }

    #[allow(unused)]
    fn extract_callable_properties_of_class(
        &mut self,
        span: Span,
        kind: ExtractKind,
        c: &ClassDef,
        prop: &Key,
        is_static_call: bool,
    ) -> VResult<Vec<CallCandidate>> {
        let mut candidates: Vec<CallCandidate> = vec![];
        for member in c.body.iter() {
            match member {
                ty::ClassMember::Method(Method {
                    key,
                    ret_ty,
                    type_params,
                    params,
                    is_static,
                    ..
                }) if *is_static == is_static_call => {
                    if self.key_matches(span, key, prop, false) {
                        candidates.push(CallCandidate {
                            type_params: type_params.as_ref().map(|v| v.params.clone()),
                            params: params.clone(),
                            ret_ty: *ret_ty.clone(),
                        });
                    }
                }
                ty::ClassMember::Property(ClassProperty { key, value, is_static, .. }) if *is_static == is_static_call => {
                    if self.key_matches(span, key, prop, false) {
                        // Check for properties with callable type.

                        // TODO(kdy1): Change error message from no callable
                        // property to property exists but not callable.

                        if let Some(prop_ty) = value.as_deref().map(Type::normalize) {
                            if let Ok(cs) = self.extract_callee_candidates(span, kind, prop_ty) {
                                candidates.extend(cs);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(candidates)
    }

    #[cfg_attr(debug_assertions, tracing::instrument(skip_all))]
    fn call_property_of_class(
        &mut self,
        span: Span,
        expr: ReevalMode,
        kind: ExtractKind,
        this: &Type,
        c: &ClassDef,
        prop: &Key,
        is_static_call: bool,
        type_args: Option<&TypeParamInstantiation>,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_ann: Option<&Type>,
        opts: CallOpts,
    ) -> VResult<Option<Type>> {
        let candidates = {
            // TODO(kdy1): Deduplicate.
            // This is duplicated intentionally because of regresions.

            let mut candidates: Vec<CallCandidate> = vec![];
            for member in c.body.iter() {
                match member {
                    ty::ClassMember::Method(Method {
                        key,
                        ret_ty,
                        type_params,
                        params,
                        is_static,
                        ..
                    }) if *is_static == is_static_call => {
                        if self.key_matches(span, key, prop, false) {
                            candidates.push(CallCandidate {
                                type_params: type_params.as_ref().map(|v| v.params.clone()),
                                params: params.clone(),
                                ret_ty: *ret_ty.clone(),
                            });
                        }
                    }
                    ty::ClassMember::Property(ClassProperty { key, value, is_static, .. }) if *is_static == is_static_call => {
                        if self.key_matches(span, key, prop, false) {
                            // Check for properties with callable type.

                            // TODO(kdy1): Change error message from no callable
                            // property to property exists but not callable.

                            if let Some(ty) = value.as_deref() {
                                return self
                                    .extract(span, expr, ty, kind, args, arg_types, spread_arg_types, type_args, type_ann, opts)
                                    .map(Some);
                            }
                        }
                    }
                    _ => {}
                }
            }

            candidates
        };

        if let Some(v) = self.select_and_invoke(
            span,
            kind,
            expr,
            &candidates,
            type_args,
            args,
            arg_types,
            spread_arg_types,
            type_ann,
            SelectOpts { ..Default::default() },
        )? {
            return Ok(Some(v));
        }

        if let Some(ty) = &c.super_class {
            let ty = if is_static_call {
                *ty.clone()
            } else {
                self.instantiate_class(span, ty)
                    .context("tried to instantiate a class to call property of a super class")?
            };
            if let Ok(ret_ty) = self.call_property(
                span,
                kind,
                expr,
                this,
                &ty,
                prop,
                type_args,
                args,
                arg_types,
                spread_arg_types,
                type_ann,
                opts,
            ) {
                return Ok(Some(ret_ty));
            }
        }

        Ok(None)
    }

    fn check_type_element_for_call<'a>(
        &mut self,
        span: Span,
        kind: ExtractKind,
        candidates: &mut Vec<CallCandidate>,
        m: &'a TypeElement,
        prop: &Key,
        opts: CallOpts,
    ) {
        let span = span.with_ctxt(SyntaxContext::empty());

        match m {
            TypeElement::Method(m) if kind == ExtractKind::Call => {
                if opts.disallow_optional_object_property && m.optional {
                    return;
                }

                if !opts.allow_private_names {
                    if m.key.is_private() || prop.is_private() {
                        return;
                    }
                }

                // We are interested only on methods named `prop`
                if let Ok(()) = self.assign(span, &mut Default::default(), &m.key.ty(), &prop.ty()) {
                    candidates.push(CallCandidate {
                        type_params: m.type_params.as_ref().map(|v| v.params.clone()),
                        params: m.params.clone(),
                        ret_ty: m
                            .ret_ty
                            .clone()
                            .map(|v| *v)
                            .unwrap_or_else(|| Type::any(m.span, Default::default())),
                    });
                }
            }

            TypeElement::Property(p) => {
                if opts.disallow_optional_object_property && p.optional {
                    // See: for-of29.ts
                    // Optional properties cannot be called.
                    return;
                }

                if self.key_matches(span, &p.key, prop, false) {
                    // TODO(kdy1): Remove useless clone
                    let ty = *p.type_ann.clone().unwrap_or(box Type::any(m.span(), Default::default()));
                    let mut ty = self
                        .normalize(Some(span), Cow::Borrowed(&ty), Default::default())
                        .map(Cow::into_owned)
                        .unwrap_or_else(|_| ty);
                    ty.normalize_mut();

                    // TODO(kdy1): PERF

                    match ty {
                        Type::Keyword(KeywordType {
                            kind: TsKeywordTypeKind::TsAnyKeyword,
                            ..
                        }) => candidates.push(CallCandidate {
                            // TODO(kdy1): Maybe we need Option<Vec<T>>.
                            params: Default::default(),
                            ret_ty: Type::any(span, Default::default()),
                            type_params: Default::default(),
                        }),

                        Type::Function(f) if kind == ExtractKind::Call => {
                            candidates.push(CallCandidate {
                                params: f.params,
                                ret_ty: *f.ret_ty,
                                type_params: f.type_params.clone().map(|v| v.params),
                            });
                        }

                        _ => {
                            if let Ok(cs) = self.extract_callee_candidates(span, kind, &ty) {
                                candidates.extend(cs);
                            }
                        }
                    }
                }
            }

            _ => {}
        }
    }

    #[cfg_attr(debug_assertions, tracing::instrument(skip_all))]
    fn call_property_of_type_elements(
        &mut self,
        kind: ExtractKind,
        expr: ReevalMode,
        span: Span,
        obj: &Type,
        members: &[TypeElement],
        prop: &Key,
        type_args: Option<&TypeParamInstantiation>,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_ann: Option<&Type>,
        opts: CallOpts,
    ) -> VResult<Type> {
        let span = span.with_ctxt(SyntaxContext::empty());

        // Candidates of the method call.
        //
        // 4 is just an unscientific guess
        // TODO(kdy1): Use smallvec
        let mut candidates = Vec::with_capacity(4);

        for m in members {
            self.check_type_element_for_call(span, kind, &mut candidates, m, prop, opts);
        }

        // TODO(kdy1): Move this to caller to prevent checking members of `Object` every
        // time we check parent interface.
        {
            // Handle methods from `interface Object`
            let i = self
                .env
                .get_global_type(span, &js_word!("Object"))
                .expect("`interface Object` is must");
            let methods = match i.normalize() {
                Type::Interface(i) => &*i.body,

                _ => &[],
            };

            // TODO(kdy1): Remove clone
            for m in methods {
                self.check_type_element_for_call(span, kind, &mut candidates, m, prop, opts);
            }
        }

        if let Some(v) = self.select_and_invoke(
            span,
            kind,
            expr,
            &candidates,
            type_args,
            args,
            arg_types,
            spread_arg_types,
            type_ann,
            SelectOpts { ..Default::default() },
        )? {
            return Ok(v);
        }

        Err(ErrorKind::NoSuchProperty {
            span,
            obj: Some(box obj.clone()),
            prop: Some(box prop.clone()),
        }
        .context("failed to call property of type elements"))
    }

    /// Returns `()`
    fn spread_args<'a>(&mut self, arg_types: &'a [TypeOrSpread]) -> VResult<Cow<'a, [TypeOrSpread]>> {
        let mut new_arg_types;

        if arg_types.iter().any(|arg| arg.spread.is_some()) {
            new_arg_types = vec![];
            for arg in arg_types {
                if arg.spread.is_some() {
                    let arg_ty = self
                        .normalize(
                            Some(arg.span()),
                            Cow::Borrowed(&arg.ty),
                            NormalizeTypeOpts {
                                preserve_global_this: true,
                                ..Default::default()
                            },
                        )
                        .context("tried to expand ref to handle a spread argument")?;
                    match arg_ty.normalize() {
                        Type::Tuple(arg_ty) => {
                            new_arg_types.extend(arg_ty.elems.iter().map(|element| &element.ty).cloned().map(|ty| TypeOrSpread {
                                span: arg.spread.unwrap(),
                                spread: None,
                                ty,
                            }));
                        }

                        Type::Keyword(KeywordType {
                            span,
                            kind: TsKeywordTypeKind::TsAnyKeyword,
                            ..
                        }) => {
                            self.scope.is_call_arg_count_unknown = true;
                            new_arg_types.push(TypeOrSpread {
                                span: *span,
                                spread: None,
                                ty: box arg_ty.clone().into_owned(),
                            });
                        }

                        Type::Array(arr) => {
                            self.scope.is_call_arg_count_unknown = true;
                            new_arg_types.push(arg.clone());
                        }

                        _ => {
                            self.scope.is_call_arg_count_unknown = true;

                            let elem_type = self
                                .get_iterator_element_type(arg.span(), arg_ty, false, Default::default())
                                .context("tried to get element type of an iterator for spread syntax in arguments")?;

                            new_arg_types.push(TypeOrSpread {
                                span: arg.span(),
                                spread: arg.spread,
                                ty: box elem_type.into_owned(),
                            });
                        }
                    }
                } else {
                    new_arg_types.push(arg.clone());
                }
            }

            new_arg_types.make_clone_cheap();

            return Ok(Cow::Owned(new_arg_types));
        } else {
            return Ok(Cow::Borrowed(arg_types));
        }
    }

    fn extract(
        &mut self,
        span: Span,
        expr: ReevalMode,
        ty: &Type,
        kind: ExtractKind,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_args: Option<&TypeParamInstantiation>,
        type_ann: Option<&Type>,
        opts: CallOpts,
    ) -> VResult<Type> {
        if !self.is_builtin {
            ty.assert_valid();
        }

        let span = span.with_ctxt(SyntaxContext::empty());

        match ty.normalize() {
            Type::Ref(..) | Type::Query(..) => {
                let ty = self.normalize(None, Cow::Borrowed(ty), Default::default())?;
                return self.extract(span, expr, &ty, kind, args, arg_types, spread_arg_types, type_args, type_ann, opts);
            }

            _ => {}
        }

        debug!("[exprs/call] Calling {}", dump_type_as_string(ty));

        if let ExtractKind::Call = kind {
            match ty.normalize() {
                Type::Interface(i) if i.name == "Function" => return Ok(Type::any(span, Default::default())),
                _ => {}
            }
        }

        if let ExtractKind::New = kind {
            match ty.normalize() {
                Type::ClassDef(ref cls) => {
                    self.scope.this = Some(Type::Class(Class {
                        span,
                        def: box cls.clone(),
                        metadata: Default::default(),
                    }));

                    if cls.is_abstract {
                        if opts.disallow_invoking_implicit_constructors {
                            return Err(ErrorKind::NoNewSignature {
                                span,
                                callee: box ty.clone(),
                            }
                            .into());
                        }

                        self.storage.report(ErrorKind::CannotCreateInstanceOfAbstractClass { span }.into());
                        // The test classAbstractInstantiation1.ts says
                        //
                        //  new A(1); // should report 1 error
                        //
                        return Ok(Type::Class(Class {
                            span,
                            def: box cls.clone(),
                            metadata: Default::default(),
                        }));
                    }

                    if let Some(type_params) = &cls.type_params {
                        for (i, param) in type_params.params.iter().enumerate() {
                            if let Some(constraint) = &param.constraint {
                                if let Some(type_args) = type_args {
                                    if let Some(type_arg) = type_args.params.get(i) {
                                        if let Err(err) = self.assign_with_opts(
                                            &mut Default::default(),
                                            constraint,
                                            type_arg,
                                            AssignOpts {
                                                span,
                                                allow_assignment_to_param_constraint: true,
                                                ..Default::default()
                                            },
                                        ) {
                                            return Err(ErrorKind::NotSatisfyConstraint {
                                                span,
                                                left: constraint.clone(),
                                                right: box type_arg.clone(),
                                            }
                                            .into());
                                        }
                                    }
                                }
                            };
                            self.register_type(param.name.clone(), Type::Param(param.clone()));
                        }
                    }

                    // Infer type arguments using constructors.
                    let mut constructors = cls
                        .body
                        .iter()
                        .filter_map(|member| match member {
                            ClassMember::Constructor(c) => Some(c),
                            _ => None,
                        })
                        .collect_vec();

                    constructors.sort_by_cached_key(|c| {
                        self.check_call_args(
                            span,
                            c.type_params.as_ref().map(|v| &*v.params),
                            &c.params,
                            type_args,
                            args,
                            arg_types,
                            spread_arg_types,
                        )
                    });

                    if let Some(constructor) = constructors.first() {
                        let type_params = constructor.type_params.as_ref().or(cls.type_params.as_deref()).map(|v| &*v.params);
                        // TODO(kdy1): Constructor's return type.

                        return self
                            .get_return_type(
                                span,
                                kind,
                                expr,
                                type_params,
                                &constructor.params,
                                Type::Class(Class {
                                    span,
                                    def: box cls.clone(),
                                    metadata: Default::default(),
                                }),
                                type_args,
                                args,
                                arg_types,
                                spread_arg_types,
                                type_ann,
                            )
                            .context("tried to instantiate a class using constructor");
                    }

                    // Check for consturctors decalred in the super class.
                    if let Some(super_class) = &cls.super_class {
                        //

                        if let Ok(v) = self.extract(
                            span,
                            expr,
                            super_class,
                            kind,
                            args,
                            arg_types,
                            spread_arg_types,
                            type_args,
                            type_ann,
                            CallOpts {
                                disallow_invoking_implicit_constructors: true,
                                ..opts
                            },
                        ) {
                            return Ok(v);
                        }
                    }

                    if opts.disallow_invoking_implicit_constructors {
                        return Err(ErrorKind::NoNewSignature {
                            span,
                            callee: box ty.clone(),
                        }
                        .into());
                    }

                    let ctx = Ctx {
                        is_instantiating_class: true,
                        ..self.ctx
                    };
                    return self
                        .with_ctx(ctx)
                        .get_return_type(
                            span,
                            kind,
                            expr,
                            cls.type_params.as_ref().map(|v| &*v.params),
                            &[],
                            Type::Class(Class {
                                span,
                                def: box cls.clone(),
                                metadata: Default::default(),
                            }),
                            type_args,
                            args,
                            arg_types,
                            spread_arg_types,
                            type_ann,
                        )
                        .context("tried to instantiate a class without any contructor with call");
                }

                Type::Constructor(c) => {
                    return self.get_return_type(
                        span,
                        kind,
                        expr,
                        c.type_params.as_ref().map(|v| &*v.params),
                        &c.params,
                        *c.type_ann.clone(),
                        type_args,
                        args,
                        arg_types,
                        spread_arg_types,
                        type_ann,
                    )
                }

                Type::This(..) => {
                    return Ok(Type::Instance(Instance {
                        span,
                        ty: box Type::This(ThisType {
                            span,
                            metadata: Default::default(),
                        }),
                        metadata: Default::default(),
                    }))
                }

                Type::Function(..) if self.rule().no_implicit_any => {
                    return Err(ErrorKind::TargetLacksConstructSignature { span }.into());
                }

                _ => {}
            }
        }

        macro_rules! ret_err {
            () => {{
                dbg!();
                match kind {
                    ExtractKind::Call => {
                        return Err(ErrorKind::NoCallSignature {
                            span,
                            callee: box ty.clone(),
                        }
                        .into())
                    }
                    ExtractKind::New => {
                        return Err(ErrorKind::NoNewSignature {
                            span,
                            callee: box ty.clone(),
                        }
                        .into())
                    }
                }
            }};
        }

        match ty.normalize() {
            Type::Intersection(..) if kind == ExtractKind::New => {
                // TODO(kdy1): Check if all types has constructor signature
                Ok(make_instance_type(ty.clone()))
            }

            Type::Keyword(KeywordType {
                kind: TsKeywordTypeKind::TsAnyKeyword,
                ..
            }) => Ok(Type::any(span, Default::default())),

            Type::Keyword(KeywordType {
                kind: TsKeywordTypeKind::TsUnknownKeyword,
                ..
            }) => {
                debug_assert!(!span.is_dummy());
                Err(ErrorKind::Unknown { span }.into())
            }

            Type::Function(ref f) if kind == ExtractKind::Call => self.get_return_type(
                span,
                kind,
                expr,
                f.type_params.as_ref().map(|v| &*v.params),
                &f.params,
                *f.ret_ty.clone(),
                type_args,
                args,
                arg_types,
                spread_arg_types,
                type_ann,
            ),

            // new fn()
            Type::Function(f) => self.get_return_type(
                span,
                kind,
                expr,
                f.type_params.as_ref().map(|v| &*v.params),
                &f.params,
                Type::any(span, Default::default()),
                type_args,
                args,
                arg_types,
                spread_arg_types,
                type_ann,
            ),

            Type::Param(TypeParam {
                constraint: Some(constraint),
                ..
            }) => self.extract(
                span,
                expr,
                constraint,
                kind,
                args,
                arg_types,
                spread_arg_types,
                type_args,
                type_ann,
                opts,
            ),

            // Type::Constructor(ty::Constructor {
            //     ref params,
            //     ref type_params,
            //     ref ret_ty,
            //     ..
            // }) if kind == ExtractKind::New => self.try_instantiate_simple(
            //     span,
            //     ty.span(),
            //     &ret_ty,
            //     params,
            //     type_params.as_ref(),
            //     args,
            //     type_args,
            // ),
            Type::Union(..) => {
                self.get_best_return_type(span, expr, ty.clone(), kind, type_args, args, arg_types, spread_arg_types, type_ann)
            }

            Type::Interface(ref i) => {
                if kind == ExtractKind::New && &**i.name.sym() == "ArrayConstructor" {
                    if let Some(type_args) = type_args {
                        if type_args.params.len() == 1 {
                            return Ok(Type::Array(Array {
                                span,
                                elem_type: box type_args.params.first().cloned().unwrap(),
                                metadata: Default::default(),
                            }));
                        }
                    }
                }

                // Search for methods
                match self.call_type_element(
                    span,
                    expr,
                    ty,
                    i.type_params.as_ref().map(|v| &*v.params),
                    &i.body,
                    kind,
                    args,
                    arg_types,
                    spread_arg_types,
                    type_args,
                    type_ann,
                ) {
                    Ok(ty) => Ok(ty),
                    Err(first_err) => {
                        //  Check parent interface
                        for parent in &i.extends {
                            let parent = self.type_of_ts_entity_name(span, &parent.expr, type_args)?;

                            if let Ok(v) = self.extract(
                                span,
                                expr,
                                &parent,
                                kind,
                                args,
                                arg_types,
                                spread_arg_types,
                                type_args,
                                type_ann,
                                opts,
                            ) {
                                return Ok(v);
                            }
                        }
                        Err(first_err)?
                    }
                }
            }

            Type::TypeLit(ref l) => self.call_type_element(
                span,
                expr,
                ty,
                None,
                &l.members,
                kind,
                args,
                arg_types,
                spread_arg_types,
                type_args,
                type_ann,
            ),

            Type::ClassDef(ref def) if kind == ExtractKind::New => {
                // TODO(kdy1): Remove clone
                Ok(Class {
                    span,
                    def: box def.clone(),
                    metadata: Default::default(),
                }
                .into())
            }

            Type::Intersection(i) => {
                // For intersection, we should select one element which matches
                // the signature

                let mut candidates = vec![];

                for ty in i.types.iter() {
                    candidates.extend(
                        self.extract_callee_candidates(span, kind, ty)
                            .context("tried to extract callable candidates from an intersection type")?,
                    );
                }

                if let Some(v) = self.select_and_invoke(
                    span,
                    kind,
                    expr,
                    &candidates,
                    type_args,
                    args,
                    arg_types,
                    spread_arg_types,
                    type_ann,
                    SelectOpts {
                        skip_check_for_overloads: true,
                        ..Default::default()
                    },
                )? {
                    return Ok(v);
                }

                ret_err!()
            }

            _ => ret_err!(),
        }
    }

    /// Search for members and returns if there's a match
    #[inline(never)]
    #[cfg_attr(debug_assertions, tracing::instrument(skip_all))]
    fn call_type_element(
        &mut self,
        span: Span,
        expr: ReevalMode,
        callee_ty: &Type,
        type_params_of_type: Option<&[TypeParam]>,
        members: &[TypeElement],
        kind: ExtractKind,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_args: Option<&TypeParamInstantiation>,
        type_ann: Option<&Type>,
    ) -> VResult<Type> {
        let callee_span = callee_ty.span();

        let candidates = members
            .iter()
            .filter_map(|member| match member {
                TypeElement::Call(CallSignature {
                    span,
                    params,
                    type_params,
                    ret_ty,
                }) if kind == ExtractKind::Call => Some(CallCandidate {
                    params: params.clone(),
                    type_params: type_params
                        .clone()
                        .map(|v| v.params)
                        .or_else(|| type_params_of_type.map(|v| v.to_vec())),
                    ret_ty: ret_ty.clone().map(|v| *v).unwrap_or_else(|| Type::any(*span, Default::default())),
                }),
                TypeElement::Constructor(ConstructorSignature {
                    span,
                    params,
                    ret_ty,
                    type_params,
                    ..
                }) if kind == ExtractKind::New => Some(CallCandidate {
                    params: params.clone(),
                    type_params: type_params
                        .clone()
                        .map(|v| v.params)
                        .or_else(|| type_params_of_type.map(|v| v.to_vec())),
                    ret_ty: ret_ty.clone().map(|v| *v).unwrap_or_else(|| Type::any(*span, Default::default())),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();

        if let Some(v) = self.select_and_invoke(
            span,
            kind,
            expr,
            &candidates,
            type_args,
            args,
            arg_types,
            spread_arg_types,
            type_ann,
            SelectOpts { ..Default::default() },
        )? {
            return Ok(v);
        }

        match kind {
            ExtractKind::Call => Err(ErrorKind::NoCallSignature {
                span,
                callee: box callee_ty.clone(),
            }
            .context("failed to select the element to invoke")),
            ExtractKind::New => Err(ErrorKind::NoNewSignature {
                span,
                callee: box callee_ty.clone(),
            }
            .context("failed to select the element to invoke")),
        }
    }

    #[allow(unused)]
    fn check_method_call(
        &mut self,
        span: Span,
        expr: ReevalMode,
        c: &MethodSignature,
        type_args: Option<&TypeParamInstantiation>,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_ann: Option<&Type>,
    ) -> VResult<Type> {
        self.get_return_type(
            span,
            ExtractKind::Call,
            expr,
            c.type_params.as_ref().map(|v| &*v.params),
            &c.params,
            c.ret_ty.clone().map(|v| *v).unwrap_or_else(|| Type::any(span, Default::default())),
            type_args,
            args,
            arg_types,
            spread_arg_types,
            type_ann,
        )
    }

    pub(super) fn extract_callee_candidates(&mut self, span: Span, kind: ExtractKind, callee: &Type) -> VResult<Vec<CallCandidate>> {
        let span = span.with_ctxt(SyntaxContext::empty());

        let callee = self
            .normalize(Some(span), Cow::Borrowed(callee), Default::default())
            .context("tried to normalize to extract callee")?;

        // TODO(kdy1): Check if signature match.
        match callee.normalize_instance() {
            Type::Intersection(i) => {
                return Ok(i
                    .types
                    .iter()
                    .map(|callee| self.extract_callee_candidates(span, kind, callee))
                    .filter_map(Result::ok)
                    .flatten()
                    .collect());
            }

            Type::Constructor(c) if kind == ExtractKind::New => {
                let candidate = CallCandidate {
                    type_params: c.type_params.clone().map(|v| v.params),
                    params: c.params.clone(),
                    ret_ty: *c.type_ann.clone(),
                };
                return Ok(vec![candidate]);
            }

            Type::Function(f) if kind == ExtractKind::Call => {
                let candidate = CallCandidate {
                    type_params: f.type_params.clone().map(|v| v.params),
                    params: f.params.clone(),
                    ret_ty: *f.ret_ty.clone(),
                };
                return Ok(vec![candidate]);
            }

            // Type::Union(ty) => {
            //     // TODO(kdy1): We should select best one based on the arugment type and count.
            //     let mut types = ty
            //         .types
            //         .iter()
            //         .cloned()
            //         .map(|callee| {
            //             self.get_best_return_type(span, callee, kind, type_args, args, arg_types,
            // spread_arg_types)         })
            //         .collect::<Result<Vec<_>, _>>()?;

            //     types.dedup_type();
            //     return Ok(Type::union(types));
            // }
            Type::Union(ref u) => {
                let candidates = u
                    .types
                    .iter()
                    .map(|callee| self.extract_callee_candidates(span, kind, callee))
                    .collect::<Result<Vec<_>, _>>()?;

                return Ok(candidates.into_iter().flatten().collect());
            }

            Type::Interface(..) => {
                let callee = self.convert_type_to_type_lit(span, callee)?.map(Cow::into_owned).map(Type::TypeLit);
                if let Some(callee) = callee {
                    return self.extract_callee_candidates(span, kind, &callee);
                }
            }

            Type::TypeLit(ty) => {
                let mut candidates = vec![];
                // Search for callable properties.
                for member in &ty.members {
                    match member {
                        TypeElement::Call(m) if kind == ExtractKind::Call => {
                            candidates.push(CallCandidate {
                                type_params: m.type_params.clone().map(|v| v.params),
                                params: m.params.clone(),
                                ret_ty: m
                                    .ret_ty
                                    .clone()
                                    .map(|v| *v)
                                    .unwrap_or_else(|| Type::any(m.span, Default::default())),
                            });
                        }

                        TypeElement::Constructor(m) if kind == ExtractKind::New => {
                            candidates.push(CallCandidate {
                                type_params: m.type_params.clone().map(|v| v.params),
                                params: m.params.clone(),
                                ret_ty: m
                                    .ret_ty
                                    .clone()
                                    .map(|v| *v)
                                    .unwrap_or_else(|| Type::any(m.span, Default::default())),
                            });
                        }
                        _ => {}
                    }
                }

                return Ok(candidates);
            }

            Type::ClassDef(cls) => {
                if kind == ExtractKind::Call {
                    return Ok(vec![]);
                }

                let mut candidates = vec![];
                for body in &cls.body {
                    if let ClassMember::Constructor(c) = body {
                        candidates.push(CallCandidate {
                            type_params: c.type_params.clone().map(|v| v.params),
                            params: c.params.clone(),
                            ret_ty: c.ret_ty.clone().map(|v| *v).unwrap_or_else(|| {
                                Type::Class(Class {
                                    span,
                                    def: box cls.clone(),
                                    metadata: Default::default(),
                                })
                            }),
                        });
                    }
                }

                if candidates.is_empty() {
                    if let Some(sc) = &cls.super_class {
                        candidates.extend(self.extract_callee_candidates(span, kind, sc)?);
                    }
                }

                if candidates.is_empty() {
                    candidates.push(CallCandidate {
                        type_params: Default::default(),
                        params: Default::default(),
                        ret_ty: Type::Class(Class {
                            span,
                            def: box cls.clone(),
                            metadata: Default::default(),
                        }),
                    });
                }

                return Ok(candidates);
            }

            _ => {}
        }

        Ok(vec![])
    }

    fn get_best_return_type(
        &mut self,
        span: Span,
        expr: ReevalMode,
        callee: Type,
        kind: ExtractKind,
        type_args: Option<&TypeParamInstantiation>,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_ann: Option<&Type>,
    ) -> VResult<Type> {
        let span = span.with_ctxt(SyntaxContext::empty());

        let has_spread = arg_types.len() != spread_arg_types.len();

        // TODO(kdy1): Calculate return type only if selected
        // This can be done by storing type params, return type, params in the
        // candidates.
        let candidates = self.extract_callee_candidates(span, kind, &callee)?;

        info!("get_best_return_type: {} candidates", candidates.len());

        if let Some(v) = self.select_and_invoke(
            span,
            kind,
            expr,
            &candidates,
            type_args,
            args,
            arg_types,
            spread_arg_types,
            type_ann,
            SelectOpts {
                skip_check_for_overloads: true,
                ..Default::default()
            },
        )? {
            return Ok(v);
        }

        if callee.is_any() {
            return Ok(Type::any(span, Default::default()));
        }

        match callee.normalize() {
            Type::ClassDef(cls) if kind == ExtractKind::New => {
                let ret_ty = self.get_return_type(
                    span,
                    kind,
                    expr,
                    cls.type_params.as_ref().map(|v| &*v.params),
                    &[],
                    callee.clone(),
                    type_args,
                    args,
                    arg_types,
                    spread_arg_types,
                    type_ann,
                )?;
                return Ok(ret_ty);
            }
            _ => {}
        }

        Err(if kind == ExtractKind::Call {
            ErrorKind::NoCallSignature { span, callee: box callee }.context("tried to calculate return type")
        } else {
            ErrorKind::NoNewSignature { span, callee: box callee }.context("tried to calculate return type")
        })
    }

    fn validate_arg_count(
        &mut self,
        span: Span,
        params: &[FnParam],
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
    ) -> VResult<()> {
        /// Count required parameter count.
        fn count_required_pat(p: &RPat) -> usize {
            match p {
                RPat::Rest(p) => {
                    if p.type_ann.is_some() {
                        return 0;
                    }

                    match &*p.arg {
                        RPat::Array(arr) => arr
                            .elems
                            .iter()
                            .map(|v| {
                                v.as_ref()
                                    .map(|pat| match pat {
                                        RPat::Array(RArrayPat { optional: false, .. })
                                        | RPat::Object(RObjectPat { optional: false, .. }) => 1,

                                        RPat::Ident(..) => 0,

                                        _ => 0,
                                    })
                                    .unwrap_or(1)
                            })
                            .sum(),
                        _ => 0,
                    }
                }
                RPat::Ident(RBindingIdent {
                    id: RIdent { sym: js_word!("this"), .. },
                    ..
                }) => 0,
                RPat::Ident(v) => usize::from(!v.id.optional),
                RPat::Array(v) => usize::from(!v.optional),
                RPat::Object(v) => usize::from(!v.optional),
                RPat::Assign(..) | RPat::Invalid(_) | RPat::Expr(_) => 0,
            }
        }

        // Assertion about deep clone
        if cfg!(debug_assertions) {
            let _p = params.to_vec();
            let _a = arg_types.to_vec();
            let _s = spread_arg_types.to_vec();
        }

        let span = span.with_ctxt(SyntaxContext::empty());

        let mut min_param: usize = params.iter().map(|v| &v.pat).map(count_required_pat).sum();

        let mut max_param = Some(params.len());
        for (index, param) in params.iter().enumerate() {
            match &param.pat {
                RPat::Rest(..) => match param.ty.normalize_instance() {
                    Type::Tuple(param_ty) => {
                        for elem in &param_ty.elems {
                            match elem.ty.normalize() {
                                Type::Rest(..) => {
                                    max_param = None;
                                    break;
                                }
                                Type::Optional(..) => {}
                                _ => {
                                    if let Some(max) = &mut max_param {
                                        *max += 1;
                                    }
                                }
                            }
                        }
                        if let Some(max) = &mut max_param {
                            *max -= 1;
                        }
                        continue;
                    }
                    _ => {
                        max_param = None;
                    }
                },
                RPat::Ident(RBindingIdent {
                    id: RIdent { sym: js_word!("this"), .. },
                    ..
                }) => {
                    if let Some(max) = &mut max_param {
                        *max -= 1;
                    }
                    continue;
                }
                _ => {}
            }
            if param.required {
                if !param.ty.is_any()
                    && self
                        .assign(
                            span,
                            &mut Default::default(),
                            &param.ty,
                            &Type::Keyword(KeywordType {
                                span,
                                kind: TsKeywordTypeKind::TsVoidKeyword,
                                metadata: Default::default(),
                            }),
                        )
                        .is_ok()
                {
                    // void is the last parameter, reduce min_params.
                    //
                    // function foo<A>(a: A, b: void) {}
                    if index == params.len() - 1 {
                        min_param -= 1;
                    }
                }
            }
        }

        let has_spread = args.iter().any(|arg| arg.spread.is_some());
        if has_spread {
            // TODO
            Ok(())
        } else {
            if min_param <= args.len() {
                if let Some(max) = max_param {
                    if args.len() <= max {
                        return Ok(());
                    }
                } else {
                    return Ok(());
                }
            }

            // For iifes, not providing some arguemnts are allowed.
            if self.ctx.is_calling_iife {
                if let Some(max) = max_param {
                    if args.len() <= max {
                        return Ok(());
                    }
                }
            }

            if max_param.is_none() {
                return Err(ErrorKind::ExpectedAtLeastNArgsButGotM { span, min: min_param }.into());
            }

            // function foo(a) {}
            // foo(1, 2, 3)
            //        ^^^^
            let span = args
                .get(min_param)
                .map(|arg| match args.last() {
                    Some(to) => arg.expr.span().to(to.expr.span()),
                    None => arg.expr.span(),
                })
                .unwrap_or(span);

            Err(ErrorKind::ExpectedNArgsButGotM {
                span,
                min: min_param,
                max: max_param,
            }
            .into())
        }
    }

    /// Returns [None] if nothing matched.
    fn select_and_invoke(
        &mut self,
        span: Span,
        kind: ExtractKind,
        expr: ReevalMode,
        candidates: &[CallCandidate],
        type_args: Option<&TypeParamInstantiation>,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_ann: Option<&Type>,
        opts: SelectOpts,
    ) -> VResult<Option<Type>> {
        let span = span.with_ctxt(SyntaxContext::empty());

        let mut callable = candidates
            .iter()
            .map(|c| {
                let res = self.check_call_args(
                    span,
                    c.type_params.as_deref(),
                    &c.params,
                    type_args,
                    args,
                    arg_types,
                    spread_arg_types,
                );

                (c, res)
            })
            .collect::<Vec<_>>();
        callable.sort_by_key(|(_, res)| *res);

        if candidates.is_empty() {
            return Ok(None);
        }

        // Check if all candidates are failed.
        if !opts.skip_check_for_overloads
            && callable.len() > 1
            && callable
                .iter()
                .all(|(_, res)| matches!(res, ArgCheckResult::WrongArgCount | ArgCheckResult::ArgTypeMismatch))
        {
            return Err(ErrorKind::NoMatchingOverload { span }.context("tried to select a call candidate"));
        }

        let (c, _) = callable.into_iter().next().unwrap();

        if candidates.len() == 1 {
            return self
                .get_return_type(
                    span,
                    kind,
                    expr,
                    c.type_params.as_deref(),
                    &c.params,
                    c.ret_ty.clone(),
                    type_args,
                    args,
                    arg_types,
                    spread_arg_types,
                    type_ann,
                )
                .map(Some);
        }

        self.get_return_type(
            span,
            kind,
            expr,
            c.type_params.as_deref(),
            &c.params,
            c.ret_ty.clone(),
            type_args,
            args,
            arg_types,
            spread_arg_types,
            type_ann,
        )
        .map(Some)
    }

    /// Returns the return type of function. This method should be called only
    /// for final step because it emits errors instead of returning them.
    ///
    /// ## Note
    ///
    /// We should evaluate two time because of code like below.
    ///
    ///
    /// ```ts
    /// declare function getType<T>(arr: T[]): string;
    /// declare function getType(obj: { foo(n: number): number[] }): string;
    /// declare function wrap<A, B>(f: (a: A) => B): (a: A) => B;
    ///
    /// getType({
    ///    foo: wrap((a) => [a.toExponential()]),
    /// })
    /// ```
    ///
    /// In this example,
    ///
    ///  - we can't calculate the type of `a.toExponential()` because we don't
    ///    know the type of `a`
    ///  - we can't use type annotation because of `wrap`
    ///  - we can't determine the function to call before validating arguments
    ///  - we can't use type annotation of the function because we cannot
    ///    determine the function to call because of `wrap`
    ///
    /// To fix this problem, we evaluate calls twice.
    ///
    /// If then, the logic becomes simple.
    ///
    ///  1. We set type of `a` to `any`.
    ///  2. Type of `a.toExponential()` is `any`.
    ///  3. Type of the arrow function is `(a: any) => [any]`.
    ///  4. Type of the property `foo` is `<A, B>(a: A) => B` where A = `any`
    /// and B = `[any]`.
    ///  5. We select appropriate function to call.
    ///  6. Type of `a` is now number.
    ///  7. Type of `a.toExponential()` is `number`.
    ///  8. Type of the arrow function is `(a: number) => [number]`.
    ///  9. Type of the property `foo` is `<A, B>(a: A) => B` where A = `number`
    /// and B = `[number]`.
    #[cfg_attr(debug_assertions, tracing::instrument(skip_all))]
    fn get_return_type(
        &mut self,
        span: Span,
        kind: ExtractKind,
        expr: ReevalMode,
        type_params: Option<&[TypeParam]>,
        params: &[FnParam],
        mut ret_ty: Type,
        type_args: Option<&TypeParamInstantiation>,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
        type_ann: Option<&Type>,
    ) -> VResult<Type> {
        let span = span.with_ctxt(SyntaxContext::empty());

        // TODO(kdy1): Optimize by skipping clone if `this type` is not used.
        let params = params
            .iter()
            .map(|param| {
                let mut ty = param.ty.clone();
                self.expand_this_in_type(&mut ty);
                ty.make_clone_cheap();
                FnParam { ty, ..param.clone() }
            })
            .collect_vec();
        self.expand_this_in_type(&mut ret_ty);

        {
            let arg_check_res = self.validate_arg_count(span, &params, args, arg_types, spread_arg_types);

            arg_check_res.report(&mut self.storage);
        }

        {
            let type_arg_check_res = self.validate_type_args_count(span, type_params, type_args);

            type_arg_check_res.report(&mut self.storage);
        }

        debug!("get_return_type: \ntype_params = {:?}\nret_ty = {:?}", type_params, ret_ty);

        if let Some(type_params) = type_params {
            // Type parameters should default to `unknown`.
            let mut default_unknown_map = HashMap::with_capacity_and_hasher(type_params.len(), Default::default());

            if type_ann.is_none() && self.ctx.reevaluating_call_or_new {
                for at in spread_arg_types {
                    if let Type::Function(Function {
                        type_params: Some(type_params),
                        ..
                    }) = at.ty.normalize()
                    {
                        for tp in type_params.params.iter() {
                            default_unknown_map.insert(
                                tp.name.clone(),
                                Type::Keyword(KeywordType {
                                    span: tp.span,
                                    kind: TsKeywordTypeKind::TsUnknownKeyword,
                                    metadata: Default::default(),
                                }),
                            );
                        }
                    }
                }
            }

            for param in type_params {
                info!("({}) Defining {}", self.scope.depth(), param.name);

                self.register_type(param.name.clone(), Type::Param(param.clone()));
            }

            let inferred_from_return_type = if self.ctx.reevaluating_call_or_new {
                None
            } else {
                match type_ann {
                    Some(type_ann) => self
                        .infer_type_with_types(span, type_params, &ret_ty, type_ann, Default::default())
                        .map(Some)?,
                    None => None,
                }
            };

            let mut expanded_params;
            let params = if let Some(map) = &inferred_from_return_type {
                expanded_params = params
                    .into_iter()
                    .map(|v| -> VResult<_> {
                        let ty = box self.expand_type_params(map, *v.ty, Default::default())?;

                        Ok(FnParam { ty, ..v })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                expanded_params.make_clone_cheap();
                expanded_params
            } else {
                params
            };

            // Assert deep clone
            if cfg!(debug_assertions) {
                let _ = type_args.cloned();
                let _ = type_params.to_vec();
                let _ = params.clone();
                let _ = spread_arg_types.to_vec();
            }

            debug!("Inferring arg types for a call");
            let mut inferred = self.infer_arg_types(
                span,
                type_args,
                type_params,
                &params,
                spread_arg_types,
                None,
                InferTypeOpts {
                    is_type_ann: type_ann.is_some(),
                    ..Default::default()
                },
            )?;
            debug!("Inferred types:\n{}", dump_type_map(&inferred.types));
            warn!("Failed to infer types of {:?}", inferred.errored);

            let expanded_param_types = params
                .into_iter()
                .map(|v| -> VResult<_> {
                    let ty = box self.expand_type_params(&inferred.types, *v.ty, Default::default())?;

                    Ok(FnParam { ty, ..v })
                })
                .collect::<Result<Vec<_>, _>>()?
                .freezed();

            let ctx = Ctx {
                in_argument: true,
                reevaluating_argument: true,
                ..self.ctx
            };
            let mut new_args = vec![];

            for (idx, (arg, param)) in args.iter().zip(expanded_param_types.iter()).enumerate() {
                let arg_ty = &arg_types[idx];
                print_type(&format!("Expanded parameter at {}", idx), &param.ty);
                print_type(&format!("Original argument at {}", idx), &arg_ty.ty);

                let (type_param_decl, actual_params) = match param.ty.normalize() {
                    Type::Function(f) => (&f.type_params, &f.params),
                    _ => {
                        new_args.push(arg_ty.clone());
                        continue;
                    }
                };

                if let Some(type_param_decl) = type_param_decl {
                    for param in &type_param_decl.params {
                        self.register_type(param.name.clone(), Type::Param(param.clone()));
                    }
                }

                // TODO: Use apply_fn_type_ann instead
                let mut patch_arg = |idx: usize, pat: &RPat| -> VResult<()> {
                    if actual_params.len() <= idx {
                        return Ok(());
                    }
                    let actual = &actual_params[idx];

                    let default_any_ty: Option<_> = try {
                        let node_id = pat.node_id()?;
                        self.mutations.as_ref()?.for_pats.get(&node_id)?.ty.clone()?
                    };

                    if let Some(ty) = default_any_ty {
                        match &ty {
                            Type::Keyword(KeywordType {
                                span,
                                kind: TsKeywordTypeKind::TsAnyKeyword,
                                metadata,
                                ..
                            }) if metadata.common.implicit => {
                                // let new_ty =
                                // RTsType::from(actual.ty.clone()).validate_with(self)?;
                                // if let Some(node_id) = pat.node_id() {
                                //     if let Some(m) = &mut self.mutations {
                                //         m.for_pats.entry(node_id).or_default().ty = Some(new_ty);
                                //     }
                                // }
                                let new_ty = *actual.ty.clone();
                                if let Some(node_id) = pat.node_id() {
                                    if let Some(m) = &mut self.mutations {
                                        m.for_pats.entry(node_id).or_default().ty = Some(new_ty);
                                    }
                                }
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                };

                let ty = match &*arg.expr {
                    RExpr::Arrow(arrow) => {
                        for (idx, pat) in arrow.params.iter().enumerate() {
                            patch_arg(idx, pat)?;
                        }

                        info!("Inferring type of arrow expr with updated type");
                        // It's okay to use default as we have patched parameters.
                        let mut ty = box Type::Function(arrow.validate_with_default(&mut *self.with_ctx(ctx))?);
                        self.add_required_type_params(&mut ty);
                        ty
                    }
                    RExpr::Fn(fn_expr) => {
                        for (idx, param) in fn_expr.function.params.iter().enumerate() {
                            patch_arg(idx, &param.pat)?;
                        }

                        info!("Inferring type of function expr with updated type");
                        let mut ty = box Type::Function(
                            fn_expr
                                .function
                                .validate_with_args(&mut *self.with_ctx(ctx), fn_expr.ident.as_ref())?,
                        );
                        self.add_required_type_params(&mut ty);
                        ty
                    }
                    _ => arg_ty.ty.clone(),
                };
                print_type(&format!("Mapped argument at {}", idx), &arg_ty.ty);

                let new_arg = TypeOrSpread { ty, ..arg_ty.clone() };

                new_args.push(new_arg);
            }

            if !self.ctx.reevaluating_call_or_new {
                debug!("Reevaluating a call");
                let ctx = Ctx {
                    reevaluating_call_or_new: true,
                    ..self.ctx
                };
                match expr {
                    ReevalMode::Call(e) => {
                        return e.validate_with_args(&mut *self.with_ctx(ctx), type_ann);
                    }
                    ReevalMode::New(e) => {
                        return e.validate_with_args(&mut *self.with_ctx(ctx), type_ann);
                    }
                    _ => {}
                }
            }

            // if arg.len() > param.len(), we need to add all args
            if arg_types.len() > expanded_param_types.len() {
                #[allow(clippy::needless_range_loop)]
                for idx in expanded_param_types.len()..arg_types.len() {
                    let ty = &arg_types[idx].ty;
                    print_type(&format!("Expanded param type at {}", idx), ty);
                }
                new_args.extend(arg_types[expanded_param_types.len()..].iter().cloned());
            }

            // We have to recalculate types.
            let mut new_arg_types;
            let spread_arg_types = if new_args.iter().any(|arg| arg.spread.is_some()) {
                new_arg_types = vec![];
                for arg in &new_args {
                    if arg.spread.is_some() {
                        match arg.ty.normalize() {
                            Type::Tuple(arg_ty) => {
                                new_arg_types.extend(arg_ty.elems.iter().map(|element| &element.ty).cloned().map(|ty| TypeOrSpread {
                                    span: arg.spread.unwrap(),
                                    spread: None,
                                    ty,
                                }));
                            }
                            _ => {
                                new_arg_types.push(arg.clone());
                            }
                        }
                    } else {
                        new_arg_types.push(arg.clone());
                    }
                }

                new_arg_types.fix();
                new_arg_types.make_clone_cheap();

                &*new_arg_types
            } else {
                new_args.fix();
                new_args.make_clone_cheap();

                &*new_args
            };

            let ctx = Ctx {
                preserve_params: true,
                preserve_ret_ty: true,
                ..self.ctx
            };
            ret_ty.fix();
            let ret_ty = self.with_ctx(ctx).expand(span, ret_ty, Default::default())?;

            for item in &expanded_param_types {
                item.ty.assert_valid();

                // Assertion for deep clones
                if cfg!(debug_assertions) {
                    let _ = item.ty.clone();
                }
            }

            for item in spread_arg_types {
                item.ty.assert_valid();

                if cfg!(debug_assertions) {
                    let _ = item.ty.clone();
                }
            }

            self.validate_arg_types(&expanded_param_types, spread_arg_types, true);

            if self.ctx.is_instantiating_class {
                for tp in type_params.iter() {
                    if !inferred.types.contains_key(&tp.name) {
                        inferred.types.insert(
                            tp.name.clone(),
                            Type::Keyword(KeywordType {
                                span: tp.span,
                                kind: TsKeywordTypeKind::TsUnknownKeyword,
                                metadata: KeywordTypeMetadata {
                                    common: tp.metadata.common,
                                    ..Default::default()
                                },
                            }),
                        );
                    }
                }
            }

            for id in &inferred.errored {
                inferred.types.insert(
                    id.clone(),
                    Type::Keyword(KeywordType {
                        span,
                        kind: TsKeywordTypeKind::TsUnknownKeyword,
                        metadata: KeywordTypeMetadata { ..Default::default() },
                    }),
                );
            }

            print_type("Return", &ret_ty);
            let mut ty = self.expand_type_params(&inferred.types, ret_ty, Default::default())?.freezed();
            print_type("Return, expanded", &ty);

            ty.visit_mut_with(&mut ReturnTypeSimplifier { analyzer: self });

            print_type("Return, simplified", &ty);

            ty = self.simplify(ty);

            print_type("Return, simplified again", &ty);

            ty = ty.fold_with(&mut ReturnTypeGeneralizer { analyzer: self });

            print_type("Return, generalized", &ty);

            self.add_required_type_params(&mut ty);

            print_type("Return, after adding type params", &ty);

            if type_ann.is_none() {
                info!("Defaulting type parameters to unknown:\n{}", dump_type_map(&default_unknown_map));

                ty = self.expand_type_params(&default_unknown_map, ty, Default::default())?;
            }

            ty.reposition(span);
            ty.make_clone_cheap();

            if kind == ExtractKind::Call {
                self.add_call_facts(&expanded_param_types, args, &mut ty);
            }

            return Ok(ty);
        }

        self.validate_arg_types(&params, spread_arg_types, type_params.is_some());

        print_type("Return", &ret_ty);

        ret_ty.reposition(span);
        ret_ty.visit_mut_with(&mut ReturnTypeSimplifier { analyzer: self });

        print_type("Return, simplified", &ret_ty);

        self.add_required_type_params(&mut ret_ty);
        ret_ty.make_clone_cheap();

        if kind == ExtractKind::Call {
            self.add_call_facts(&params, args, &mut ret_ty);
        }

        Ok(ret_ty)
    }

    fn validate_arg_types(&mut self, params: &[FnParam], spread_arg_types: &[TypeOrSpread], is_generic: bool) {
        info!("[exprs] Validating arguments");

        macro_rules! report_err {
            ($err:expr) => {{
                self.storage.report($err.into());
                if is_generic {
                    return;
                }
            }};
        }

        let marks = self.marks();

        let rest_idx = {
            let mut rest_idx = None;
            let mut shift = 0;

            for (idx, param) in params.iter().enumerate() {
                match param.pat {
                    RPat::Rest(..) => {
                        rest_idx = Some(idx - shift);
                    }
                    _ => {
                        if !param.required {
                            shift += 1;
                        }
                    }
                }
            }

            rest_idx
        };

        for (idx, arg) in spread_arg_types.iter().enumerate() {
            if arg.spread.is_some() {
                if let Some(rest_idx) = rest_idx {
                    if idx < rest_idx {
                        match arg.ty.normalize() {
                            Type::Tuple(..) => {
                                report_err!(ErrorKind::ExpectedAtLeastNArgsButGotMOrMore {
                                    span: arg.span(),
                                    min: rest_idx - 1,
                                })
                            }

                            _ => {
                                report_err!(ErrorKind::SpreadMustBeTupleOrPassedToRest { span: arg.span() })
                            }
                        }
                    }
                }
            }
        }

        let mut params_iter = params.iter().filter(|param| {
            !matches!(
                param.pat,
                RPat::Ident(RBindingIdent {
                    id: RIdent { sym: js_word!("this"), .. },
                    ..
                })
            )
        });
        let mut args_iter = spread_arg_types.iter();

        loop {
            let param = params_iter.next();
            let arg = args_iter.next();

            if param.is_none() || arg.is_none() {
                break;
            }

            if let (Some(param), Some(arg)) = (param, arg) {
                if let RPat::Rest(..) = &param.pat {
                    let param_ty = self.normalize(Some(arg.span()), Cow::Borrowed(&param.ty), Default::default());

                    let param_ty = match param_ty {
                        Ok(v) => v,
                        Err(err) => {
                            report_err!(err);
                            continue;
                        }
                    }
                    .freezed();

                    // Handle
                    //
                    //   param: (...x: [boolean, sting, ...number])
                    //   arg: (true, 'str')
                    //      or
                    //   arg: (true, 'str', 10)
                    if arg.spread.is_none() {
                        match param_ty.normalize() {
                            Type::Tuple(param_ty) if !param_ty.elems.is_empty() => {
                                let res = self
                                    .assign_with_opts(
                                        &mut Default::default(),
                                        &param_ty.elems[0].ty,
                                        &arg.ty,
                                        AssignOpts {
                                            span: arg.span(),
                                            allow_iterable_on_rhs: true,
                                            ..Default::default()
                                        },
                                    )
                                    .convert_err(|err| ErrorKind::WrongArgType {
                                        span: arg.span(),
                                        inner: box err.into(),
                                    })
                                    .context("tried to assign to first element of a tuple type of a parameter");

                                match res {
                                    Ok(_) => {}
                                    Err(err) => {
                                        report_err!(err);
                                        continue;
                                    }
                                };

                                for param_elem in param_ty.elems.iter().skip(1) {
                                    let arg = match args_iter.next() {
                                        Some(v) => v,
                                        None => {
                                            // TODO(kdy1): Arugment count
                                            break;
                                        }
                                    };

                                    // TODO(kdy1): Check if arg.spread is none.
                                    // The logic below is correct only if the arg is not
                                    // spread.

                                    let res = self
                                        .assign_with_opts(
                                            &mut Default::default(),
                                            &param_elem.ty,
                                            &arg.ty,
                                            AssignOpts {
                                                span: arg.span(),
                                                allow_iterable_on_rhs: true,
                                                ..Default::default()
                                            },
                                        )
                                        .convert_err(|err| ErrorKind::WrongArgType {
                                            span: arg.span(),
                                            inner: box err.into(),
                                        })
                                        .context("tried to assign to element of a tuple type of a parameter");

                                    match res {
                                        Ok(_) => {}
                                        Err(err) => {
                                            report_err!(err);
                                            continue;
                                        }
                                    };
                                }

                                // Skip default type checking logic.
                                continue;
                            }
                            _ => {}
                        }
                    }

                    if arg.spread.is_some() {
                        if let Ok(()) = self.assign_with_opts(
                            &mut Default::default(),
                            &param.ty,
                            &arg.ty,
                            AssignOpts {
                                span: arg.span(),
                                allow_iterable_on_rhs: true,
                                ..Default::default()
                            },
                        ) {
                            continue;
                        }
                    }

                    match param_ty.normalize() {
                        Type::Array(arr) => {
                            // We should change type if the parameter is a rest parameter.
                            let res = self.assign(arg.span(), &mut Default::default(), &arr.elem_type, &arg.ty);
                            let err = match res {
                                Ok(()) => continue,
                                Err(err) => err,
                            };

                            let err = err
                                .convert(|err| ErrorKind::WrongArgType {
                                    span: arg.span(),
                                    inner: box err.into(),
                                })
                                .context("tried assigning elem type of an array because parameter is declared as a rest pattern");
                            report_err!(err);
                            continue;
                        }
                        _ => {
                            if let Ok(()) = self.assign_with_opts(
                                &mut Default::default(),
                                &param.ty,
                                &arg.ty,
                                AssignOpts {
                                    span: arg.span(),
                                    allow_iterable_on_rhs: true,
                                    ..Default::default()
                                },
                            ) {
                                continue;
                            }
                        }
                    }
                }

                if arg.spread.is_some() {
                    let res = self.get_iterator_element_type(arg.span(), Cow::Borrowed(&arg.ty), false, Default::default());
                    match res {
                        Ok(arg_elem_ty) => {
                            // We should change type if the parameter is a rest parameter.
                            if let Ok(()) = self.assign(arg.span(), &mut Default::default(), &param.ty, &arg_elem_ty) {
                                continue;
                            }
                        }
                        Err(err) => {
                            if let ErrorKind::MustHaveSymbolIteratorThatReturnsIterator { span } = &*err {
                                report_err!(ErrorKind::SpreadMustBeTupleOrPassedToRest { span: *span });
                                continue;
                            }
                        }
                    }

                    let res = self
                        .assign_with_opts(
                            &mut Default::default(),
                            &param.ty,
                            &arg.ty,
                            AssignOpts {
                                span: arg.span(),
                                ..Default::default()
                            },
                        )
                        .convert_err(|err| ErrorKind::WrongArgType {
                            span: err.span(),
                            inner: box err.into(),
                        })
                        .context("arg is spread");
                    if let Err(err) = res {
                        report_err!(err);
                    }
                } else {
                    let allow_unknown_rhs = arg.ty.metadata().resolved_from_var || !matches!(arg.ty.normalize(), Type::TypeLit(..));
                    if let Err(err) = self.assign_with_opts(
                        &mut Default::default(),
                        &param.ty,
                        &arg.ty,
                        AssignOpts {
                            span: arg.span(),
                            allow_unknown_rhs: Some(allow_unknown_rhs),
                            use_missing_fields_for_class: true,
                            ..Default::default()
                        },
                    ) {
                        let err = err.convert(|err| {
                            match err {
                                ErrorKind::TupleAssignError { span, errors } if !arg.ty.metadata().resolved_from_var => {
                                    return ErrorKind::Errors { span, errors }
                                }
                                ErrorKind::ObjectAssignFailed { span, errors } if !arg.ty.metadata().resolved_from_var => {
                                    return ErrorKind::Errors { span, errors }
                                }
                                ErrorKind::Errors { span, ref errors } => {
                                    if errors
                                        .iter()
                                        .all(|err| matches!(&**err, ErrorKind::UnknownPropertyInObjectLiteralAssignment { span }))
                                    {
                                        return ErrorKind::Errors {
                                            span,
                                            errors: errors
                                                .iter()
                                                .map(|err| {
                                                    ErrorKind::WrongArgType {
                                                        span: err.span(),
                                                        inner: box err.clone(),
                                                    }
                                                    .into()
                                                })
                                                .collect(),
                                        };
                                    }
                                }
                                _ => {}
                            }

                            ErrorKind::WrongArgType {
                                span: arg.span(),
                                inner: box err.into(),
                            }
                        });

                        report_err!(err);
                    }
                }
            }
        }
    }

    /// Note:
    ///
    /// ```ts
    /// function isSubscriber(val: any): val is DummySubscriber;
    /// const observerOrNext: () => void | Subscriber;
    /// const subscriber = isSubscriber(observerOrNext) ? observerOrNext : new SafeSubscriber();
    /// ```
    ///
    /// should make type of `subscriber` `SafeSubscriber`, not `Subscriber`.
    /// I (kdy1) don't know why.
    fn add_call_facts(&mut self, params: &[FnParam], args: &[RExprOrSpread], ret_ty: &mut Type) {
        if let Type::Predicate(p) = ret_ty.normalize() {
            let ty = match &p.ty {
                Some(v) => v.normalize(),
                None => return,
            };

            match &p.param_name {
                RTsThisTypeOrIdent::TsThisType(this) => {}
                RTsThisTypeOrIdent::Ident(arg_id) => {
                    for (idx, param) in params.iter().enumerate() {
                        match &param.pat {
                            RPat::Ident(i) if i.id.sym == arg_id.sym => {
                                // TODO(kdy1): Check length of args.
                                let arg = &args[idx];
                                if let RExpr::Ident(var_name) = &*arg.expr {
                                    let ty = ty.clone().freezed();
                                    self.store_call_fact_for_var(var_name.span, var_name.into(), &ty);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn narrow_with_predicate(&mut self, span: Span, orig_ty: &Type, new_ty: Type) -> VResult<Type> {
        let span = span.with_ctxt(SyntaxContext::empty());

        let orig_ty = self
            .normalize(Some(span), Cow::Borrowed(orig_ty), Default::default())
            .context("tried to normalize original type")?
            .freezed();
        let new_ty = self
            .normalize(Some(span), Cow::Owned(new_ty), Default::default())
            .context("tried to normalize new type")?
            .freezed();

        let use_simple_intersection = (|| {
            if let (Type::Interface(orig), Type::Interface(new)) = (orig_ty.normalize(), new_ty.normalize()) {
                if orig.extends.is_empty() && new.extends.is_empty() {
                    return true;
                }
            }

            false
        })();

        if use_simple_intersection {
            return Ok(Type::Intersection(Intersection {
                span,
                types: vec![orig_ty.into_owned(), new_ty.into_owned()],
                metadata: Default::default(),
            }));
        }

        match new_ty.normalize() {
            Type::Keyword(..) | Type::Lit(..) => {}
            _ => {
                match orig_ty.normalize() {
                    Type::Union(..) | Type::Interface(..) => {}

                    _ => {
                        if let Some(v) = self.extends(span, &orig_ty, &new_ty, Default::default()) {
                            if v {
                                if let Type::ClassDef(def) = orig_ty.normalize() {
                                    return Ok(Type::Class(Class {
                                        span,
                                        def: box def.clone(),
                                        metadata: Default::default(),
                                    }));
                                }
                                return Ok(orig_ty.into_owned());
                            }
                        }

                        return Ok(new_ty.into_owned());
                    }
                }

                let mut new_types = vec![];

                let mut upcasted = false;
                for ty in orig_ty.iter_union() {
                    if let Some(true) = self.extends(span, &new_ty, ty, Default::default()) {
                        upcasted = true;
                        new_types.push(new_ty.clone().into_owned());
                    } else if let Some(true) = self.extends(span, ty, &new_ty, Default::default()) {
                        new_types.push(ty.clone());
                    }
                }

                // TODO(kdy1): Use super class instread of
                if !upcasted && new_types.is_empty() {
                    new_types.push(new_ty.clone().into_owned());
                }

                new_types.dedup_type();
                let mut new_ty = Type::new_union_without_dedup(span, new_types);
                if upcasted {
                    new_ty.metadata_mut().prevent_converting_to_children = true;
                }
                return Ok(new_ty);
            }
        }

        if let Type::ClassDef(def) = new_ty.normalize() {
            return Ok(Type::Class(Class {
                span,
                def: box def.clone(),
                metadata: Default::default(),
            }));
        }

        Ok(new_ty.into_owned())
    }

    #[extra_validator]
    fn store_call_fact_for_var(&mut self, span: Span, var_name: Id, new_ty: &Type) {
        match new_ty.normalize() {
            Type::Keyword(..) | Type::Lit(..) => {}
            _ => {
                if let Some(previous_types) = self.find_var_type(&var_name.clone(), TypeOfMode::RValue).map(Cow::into_owned) {
                    let narrowed_ty = self.narrow_with_predicate(span, &previous_types, new_ty.clone())?.freezed();

                    self.add_type_fact(&var_name, narrowed_ty, new_ty.clone());
                    return;
                }
            }
        }

        let new_ty = new_ty.clone().freezed();
        self.add_type_fact(&var_name, new_ty.clone(), new_ty);
    }

    pub(crate) fn validate_type_args_count(
        &mut self,
        span: Span,
        type_params: Option<&[TypeParam]>,
        type_args: Option<&TypeParamInstantiation>,
    ) -> VResult<()> {
        if let Some(type_params) = type_params {
            if let Some(type_args) = type_args {
                // TODO(kdy1): Handle defaults of the type parameter (Change to range)
                if type_params.len() != type_args.params.len() {
                    return Err(ErrorKind::TypeParameterCountMismatch {
                        span,
                        max: type_params.len(),
                        min: type_params.len(),
                        actual: type_args.params.len(),
                    }
                    .into());
                }
            }
        }

        Ok(())
    }

    fn is_subtype_in_fn_call(&mut self, span: Span, arg: &Type, param: &Type) -> bool {
        if arg.type_eq(param) {
            return true;
        }

        if param.is_any() {
            return true;
        }

        if arg.is_any() {
            return false;
        }

        self.assign(span, &mut Default::default(), arg, param).is_ok()
    }

    /// This method return [Err] if call is invalid
    ///
    ///
    /// # Implementation notes
    ///
    /// `anyAssignabilityInInheritance.ts` says `any, not a subtype of number so
    /// it skips that overload, is a subtype of itself so it picks second (if
    /// truly ambiguous it would pick first overload)`
    fn check_call_args(
        &mut self,
        span: Span,
        type_params: Option<&[TypeParam]>,
        params: &[FnParam],
        type_args: Option<&TypeParamInstantiation>,
        args: &[RExprOrSpread],
        arg_types: &[TypeOrSpread],
        spread_arg_types: &[TypeOrSpread],
    ) -> ArgCheckResult {
        if self.validate_type_args_count(span, type_params, type_args).is_err() {
            return ArgCheckResult::WrongArgCount;
        }

        if self.validate_arg_count(span, params, args, arg_types, spread_arg_types).is_err() {
            return ArgCheckResult::WrongArgCount;
        }

        self.with_scope_for_type_params(|analyzer: &mut Analyzer| {
            if let Some(type_params) = type_params {
                for param in type_params {
                    analyzer.register_type(param.name.clone(), Type::Param(param.clone()));
                }
            }

            let mut exact = true;

            for (arg, param) in arg_types.iter().zip(params) {
                // match arg.ty.normalize() {
                //     Type::Union(..) => match param.ty.normalize() {
                //         Type::Keyword(..) => if self.assign(&param.ty, &arg.ty, span).is_ok()
                // {},         _ => {}
                //     },
                //     _ => {}
                // }

                match param.ty.normalize() {
                    Type::Param(..) => {}
                    Type::Instance(param) if param.ty.is_type_param() => {}
                    _ => {
                        if analyzer
                            .assign_with_opts(
                                &mut Default::default(),
                                &param.ty,
                                &arg.ty,
                                AssignOpts {
                                    span,
                                    allow_unknown_rhs: Some(true),
                                    allow_assignment_to_param: true,
                                    ..Default::default()
                                },
                            )
                            .is_err()
                        {
                            return ArgCheckResult::ArgTypeMismatch;
                        }

                        if !analyzer.is_subtype_in_fn_call(span, &arg.ty, &param.ty) {
                            exact = false;
                        }
                    }
                }
            }

            if analyzer.scope.is_call_arg_count_unknown || !exact {
                return ArgCheckResult::MayBe;
            }

            ArgCheckResult::Exact
        })
    }

    fn apply_type_ann_from_callee(&mut self, span: Span, kind: ExtractKind, args: &[RExprOrSpread], callee: &Type) -> VResult<()> {
        let c = self.extract_callee_candidates(span, kind, callee)?;

        if c.len() != 1 {
            return Ok(());
        }

        let c = c.into_iter().next().unwrap();

        // TODO(kdy1): Refactor generic inference logic to use this function.
        // Currently, the reevaluation logic in get_return_type interferes with this
        // function
        if c.type_params.is_some() {
            return Ok(());
        }

        for (arg, param) in args.iter().zip(c.params.iter()) {
            // TODO(kdy1):  Handle rest
            if arg.spread.is_some() || matches!(param.pat, RPat::Rest(..)) {
                break;
            }

            self.apply_type_ann_for_arg(&arg.expr, &param.ty)?;
        }

        Ok(())
    }

    fn apply_type_ann_for_arg(&mut self, arg: &RExpr, type_ann: &Type) -> VResult<()> {
        match arg {
            RExpr::Paren(arg) => return self.apply_type_ann_for_arg(&arg.expr, type_ann),
            RExpr::Fn(arg) => {
                self.apply_fn_type_ann(arg.span(), arg.function.params.iter().map(|v| &v.pat), Some(type_ann));
            }
            RExpr::Arrow(arg) => {
                self.apply_fn_type_ann(arg.span(), arg.params.iter(), Some(type_ann));
            }
            _ => {}
        }

        Ok(())
    }

    fn validate_args(&mut self, args: &[RExprOrSpread]) -> VResult<Vec<TypeOrSpread>> {
        let ctx = Ctx {
            in_argument: true,
            should_store_truthy_for_access: false,
            ..self.ctx
        };
        self.with_ctx(ctx).with(|this: &mut Analyzer| {
            let args: Vec<_> = args
                .iter()
                .map(|arg| {
                    arg.validate_with(this).report(&mut this.storage).unwrap_or_else(|| TypeOrSpread {
                        span: arg.span(),
                        spread: arg.spread,
                        ty: box Type::any(arg.expr.span(), Default::default()),
                    })
                })
                .collect();

            Ok(args)
        })
    }
}

/// Used for reevaluation.
#[derive(Clone, Copy)]
pub(crate) enum ReevalMode<'a> {
    Call(&'a RCallExpr),
    New(&'a RNewExpr),
    NoReeval,
}

impl Default for ReevalMode<'_> {
    fn default() -> Self {
        Self::NoReeval
    }
}

struct ReturnTypeGeneralizer<'a, 'b, 'c> {
    analyzer: &'a mut Analyzer<'b, 'c>,
}

impl Fold<Type> for ReturnTypeGeneralizer<'_, '_, '_> {
    fn fold(&mut self, mut ty: Type) -> Type {
        if !self.analyzer.may_generalize(&ty) {
            return ty;
        }

        // TODO(kdy1): PERF
        ty.normalize_mut();

        ty = ty.fold_children_with(self);

        ty.generalize_lit()
    }
}

///
/// e.g.
///
/// - `any[string]` => `any`
/// - `Shape['name']` => `string`
struct ReturnTypeSimplifier<'a, 'b, 'c> {
    analyzer: &'a mut Analyzer<'b, 'c>,
}

impl VisitMut<Type> for ReturnTypeSimplifier<'_, '_, '_> {
    fn visit_mut(&mut self, ty: &mut Type) {
        // TODO(kdy1): PERF
        ty.normalize_mut();

        ty.visit_mut_children_with(self);

        match ty {
            Type::IndexedAccessType(IndexedAccessType {
                obj_type:
                    box Type::Keyword(KeywordType {
                        span,
                        kind: TsKeywordTypeKind::TsAnyKeyword,
                        metadata,
                    }),
                ..
            }) => {
                *ty = Type::Keyword(KeywordType {
                    span: *span,
                    kind: TsKeywordTypeKind::TsAnyKeyword,
                    metadata: *metadata,
                });
            }

            Type::IndexedAccessType(IndexedAccessType {
                span,
                obj_type: ref obj_ty @ box Type::Ref(..),
                index_type,
                metadata,
                ..
            }) if is_str_lit_or_union(index_type) => {
                let mut types: Vec<Type> = vec![];

                for index_ty in index_type.iter_union() {
                    let (lit_span, value) = match index_ty.normalize() {
                        Type::Lit(LitType {
                            span: lit_span,
                            lit: RTsLit::Str(RStr { value, .. }),
                            ..
                        }) => (*lit_span, value.clone()),
                        _ => return,
                    };

                    let ctx = Ctx {
                        preserve_ref: false,
                        ignore_expand_prevention_for_top: true,
                        ..self.analyzer.ctx
                    };
                    let mut a = self.analyzer.with_ctx(ctx);
                    let obj = a
                        .expand(
                            *span,
                            *obj_ty.clone(),
                            ExpandOpts {
                                full: true,
                                expand_union: true,
                                ..Default::default()
                            },
                        )
                        .report(&mut a.storage);
                    if let Some(obj) = &obj {
                        if let Some(actual_ty) = a
                            .access_property(
                                *span,
                                obj,
                                &Key::Normal {
                                    span: lit_span,
                                    sym: value.clone(),
                                },
                                TypeOfMode::RValue,
                                IdCtx::Type,
                                Default::default(),
                            )
                            .context("tried to access property to simplify return type")
                            .report(&mut a.storage)
                        {
                            if types.iter().all(|prev_ty| !(*prev_ty).type_eq(&actual_ty)) {
                                types.push(actual_ty);
                            }
                        }
                    }
                }

                *ty = Type::Union(Union {
                    span: *span,
                    types,
                    metadata: UnionMetadata {
                        common: metadata.common,
                        ..Default::default()
                    },
                })
                .fixed();
            }

            Type::IndexedAccessType(ty) if is_str_lit_or_union(&ty.index_type) => {
                prevent_generalize(ty);
            }

            // Boxified<A | B | C> => Boxified<A> | Boxified<B> | Boxified<C>
            Type::Ref(Ref {
                span,
                type_name: RTsEntityName::Ident(i),
                type_args: Some(type_args),
                metadata,
            }) if type_args.params.len() == 1 && type_args.params.iter().any(|ty| matches!(ty.normalize(), Type::Union(..))) => {
                // TODO(kdy1): Replace .ok() with something better
                if let Some(types) = self.analyzer.find_type(&(&*i).into()).ok().flatten() {
                    type_args.make_clone_cheap();

                    for stored_ty in types {
                        if let Type::Alias(Alias { ty: aliased_ty, .. }) = stored_ty.normalize() {
                            let mut types = vec![];

                            if let Type::Union(type_arg) = &type_args.params[0].normalize() {
                                for ty in &type_arg.types {
                                    types.push(Type::Ref(Ref {
                                        span: *span,
                                        type_name: RTsEntityName::Ident(i.clone()),
                                        type_args: Some(box TypeParamInstantiation {
                                            span: type_args.span,
                                            params: vec![ty.clone()],
                                        }),
                                        metadata: *metadata,
                                    }))
                                }
                            } else {
                                unreachable!()
                            }

                            *ty = Type::union(types);
                            return;
                        }
                    }
                }
            }

            _ => {}
        }
    }
}

fn is_fn_expr(callee: &RExpr) -> bool {
    match callee {
        RExpr::Arrow(..) | RExpr::Fn(..) => true,
        RExpr::Paren(e) => is_fn_expr(&e.expr),
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Ord)]
enum ArgCheckResult {
    Exact,
    MayBe,
    ArgTypeMismatch,
    WrongArgCount,
}

#[derive(Debug, Default, Clone, Copy)]
struct SelectOpts {
    /// Defaults to false.
    skip_check_for_overloads: bool,
}

/// Ensure that sort work as expected.
#[test]
fn test_arg_check_result_order() {
    let mut v = vec![
        ArgCheckResult::Exact,
        ArgCheckResult::MayBe,
        ArgCheckResult::ArgTypeMismatch,
        ArgCheckResult::WrongArgCount,
    ];
    let expected = v.clone();
    v.sort();

    assert_eq!(v, expected);
}

/// TODO(kdy1): Use cow
pub(super) struct CallCandidate {
    pub type_params: Option<Vec<TypeParam>>,
    pub params: Vec<FnParam>,
    pub ret_ty: Type,
}
