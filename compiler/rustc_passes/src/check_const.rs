//! This pass checks HIR bodies that may be evaluated at compile-time (e.g., `const`, `static`,
//! `const fn`) for structured control flow (e.g. `if`, `while`), which is forbidden in a const
//! context.
//!
//! By the time the MIR const-checker runs, these high-level constructs have been lowered to
//! control-flow primitives (e.g., `Goto`, `SwitchInt`), making it tough to properly attribute
//! errors. We still look for those primitives in the MIR const-checker to ensure nothing slips
//! through, but errors for structured control flow in a `const` should be emitted here.

use rustc_attr as attr;
use rustc_errors::struct_span_err;
use rustc_hir as hir;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::{self, Visitor};
use rustc_middle::hir::nested_filter;
use rustc_middle::ty;
use rustc_middle::ty::query::Providers;
use rustc_middle::ty::TyCtxt;
use rustc_session::parse::feature_err;
use rustc_span::{sym, Span, Symbol};

/// An expression that is not *always* legal in a const context.
#[derive(Clone, Copy)]
enum NonConstExpr {
    Loop(hir::LoopSource),
    Match(hir::MatchSource),
}

impl NonConstExpr {
    fn name(self) -> String {
        match self {
            Self::Loop(src) => format!("`{}`", src.name()),
            Self::Match(src) => format!("`{}`", src.name()),
        }
    }

    fn required_feature_gates(self) -> Option<&'static [Symbol]> {
        use hir::LoopSource::*;
        use hir::MatchSource::*;

        let gates: &[_] = match self {
            Self::Match(AwaitDesugar) => {
                return None;
            }

            Self::Loop(ForLoop) | Self::Match(ForLoopDesugar) => &[sym::const_for],

            Self::Match(TryDesugar) => &[sym::const_try],

            // All other expressions are allowed.
            Self::Loop(Loop | While) | Self::Match(Normal) => &[],
        };

        Some(gates)
    }
}

fn check_mod_const_bodies(tcx: TyCtxt<'_>, module_def_id: LocalDefId) {
    let mut vis = CheckConstVisitor::new(tcx);
    tcx.hir().deep_visit_item_likes_in_module(module_def_id, &mut vis);
}

pub(crate) fn provide(providers: &mut Providers) {
    *providers = Providers { check_mod_const_bodies, ..*providers };
}

fn check_item<'tcx>(tcx: TyCtxt<'tcx>, item: &'tcx hir::Item<'tcx>) {
    let _: Option<_> = try {
        if let hir::ItemKind::Impl(ref imp) = item.kind && let hir::Constness::Const = imp.constness {
            let trait_def_id = imp.of_trait.as_ref()?.trait_def_id()?;
            let ancestors = tcx
                .trait_def(trait_def_id)
                .ancestors(tcx, item.def_id.to_def_id())
                .ok()?;
            let mut to_implement = Vec::new();

            for trait_item in tcx.associated_items(trait_def_id).in_definition_order()
            {
                if let ty::AssocItem {
                    kind: ty::AssocKind::Fn,
                    defaultness,
                    def_id: trait_item_id,
                    ..
                } = *trait_item
                {
                    // we can ignore functions that do not have default bodies:
                    // if those are unimplemented it will be caught by typeck.
                    if !defaultness.has_value()
                        || tcx
                        .has_attr(trait_item_id, sym::default_method_body_is_const)
                    {
                        continue;
                    }

                    let is_implemented = ancestors
                        .leaf_def(tcx, trait_item_id)
                        .map(|node_item| !node_item.defining_node.is_from_trait())
                        .unwrap_or(false);

                    if !is_implemented {
                        to_implement.push(trait_item_id);
                    }
                }
            }

            // all nonconst trait functions (not marked with #[default_method_body_is_const])
            // must be implemented
            if !to_implement.is_empty() {
                let not_implemented = to_implement
                    .into_iter()
                    .map(|did| tcx.item_name(did).to_string())
                    .collect::<Vec<_>>()
                    .join("`, `");
                tcx
                    .sess
                    .struct_span_err(
                        item.span,
                        "const trait implementations may not use non-const default functions",
                    )
                    .note(&format!("`{}` not implemented", not_implemented))
                    .emit();
            }
        }
    };
}

#[derive(Copy, Clone)]
struct CheckConstVisitor<'tcx> {
    tcx: TyCtxt<'tcx>,
    const_kind: Option<hir::ConstContext>,
    def_id: Option<LocalDefId>,
}

impl<'tcx> CheckConstVisitor<'tcx> {
    fn new(tcx: TyCtxt<'tcx>) -> Self {
        CheckConstVisitor { tcx, const_kind: None, def_id: None }
    }

    /// Emits an error when an unsupported expression is found in a const context.
    fn const_check_violated(&self, expr: NonConstExpr, span: Span) {
        let Self { tcx, def_id, const_kind } = *self;

        let features = tcx.features();
        let required_gates = expr.required_feature_gates();

        let is_feature_allowed = |feature_gate| {
            // All features require that the corresponding gate be enabled,
            // even if the function has `#[rustc_allow_const_fn_unstable(the_gate)]`.
            if !tcx.features().enabled(feature_gate) {
                return false;
            }

            // If `def_id` is `None`, we don't need to consider stability attributes.
            let def_id = match def_id {
                Some(x) => x,
                None => return true,
            };

            // If the function belongs to a trait, then it must enable the const_trait_impl
            // feature to use that trait function (with a const default body).
            if tcx.trait_of_item(def_id).is_some() {
                return true;
            }

            // If this crate is not using stability attributes, or this function is not claiming to be a
            // stable `const fn`, that is all that is required.
            if !tcx.features().staged_api
                || tcx.has_attr(def_id.to_def_id(), sym::rustc_const_unstable)
            {
                return true;
            }

            // However, we cannot allow stable `const fn`s to use unstable features without an explicit
            // opt-in via `rustc_allow_const_fn_unstable`.
            let attrs = tcx.hir().attrs(tcx.hir().local_def_id_to_hir_id(def_id));
            attr::rustc_allow_const_fn_unstable(&tcx.sess, attrs).any(|name| name == feature_gate)
        };

        match required_gates {
            // Don't emit an error if the user has enabled the requisite feature gates.
            Some(gates) if gates.iter().copied().all(is_feature_allowed) => return,

            // `-Zunleash-the-miri-inside-of-you` only works for expressions that don't have a
            // corresponding feature gate. This encourages nightly users to use feature gates when
            // possible.
            None if tcx.sess.opts.debugging_opts.unleash_the_miri_inside_of_you => {
                tcx.sess.span_warn(span, "skipping const checks");
                return;
            }

            _ => {}
        }

        let const_kind =
            const_kind.expect("`const_check_violated` may only be called inside a const context");

        let msg = format!("{} is not allowed in a `{}`", expr.name(), const_kind.keyword_name());

        let required_gates = required_gates.unwrap_or(&[]);
        let missing_gates: Vec<_> =
            required_gates.iter().copied().filter(|&g| !features.enabled(g)).collect();

        match missing_gates.as_slice() {
            [] => {
                struct_span_err!(tcx.sess, span, E0744, "{}", msg).emit();
            }

            [missing_primary, ref missing_secondary @ ..] => {
                let mut err = feature_err(&tcx.sess.parse_sess, *missing_primary, span, &msg);

                // If multiple feature gates would be required to enable this expression, include
                // them as help messages. Don't emit a separate error for each missing feature gate.
                //
                // FIXME(ecstaticmorse): Maybe this could be incorporated into `feature_err`? This
                // is a pretty narrow case, however.
                if tcx.sess.is_nightly_build() {
                    for gate in missing_secondary {
                        let note = format!(
                            "add `#![feature({})]` to the crate attributes to enable",
                            gate,
                        );
                        err.help(&note);
                    }
                }

                err.emit();
            }
        }
    }

    /// Saves the parent `const_kind` before calling `f` and restores it afterwards.
    fn recurse_into(
        &mut self,
        kind: Option<hir::ConstContext>,
        def_id: Option<LocalDefId>,
        f: impl FnOnce(&mut Self),
    ) {
        let parent_def_id = self.def_id;
        let parent_kind = self.const_kind;
        self.def_id = def_id;
        self.const_kind = kind;
        f(self);
        self.def_id = parent_def_id;
        self.const_kind = parent_kind;
    }
}

impl<'tcx> Visitor<'tcx> for CheckConstVisitor<'tcx> {
    type NestedFilter = nested_filter::OnlyBodies;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.tcx.hir()
    }

    fn visit_item(&mut self, item: &'tcx hir::Item<'tcx>) {
        intravisit::walk_item(self, item);
        check_item(self.tcx, item);
    }

    fn visit_anon_const(&mut self, anon: &'tcx hir::AnonConst) {
        let kind = Some(hir::ConstContext::Const);
        self.recurse_into(kind, None, |this| intravisit::walk_anon_const(this, anon));
    }

    fn visit_body(&mut self, body: &'tcx hir::Body<'tcx>) {
        let owner = self.tcx.hir().body_owner_def_id(body.id());
        let kind = self.tcx.hir().body_const_context(owner);
        self.recurse_into(kind, Some(owner), |this| intravisit::walk_body(this, body));
    }

    fn visit_expr(&mut self, e: &'tcx hir::Expr<'tcx>) {
        match &e.kind {
            // Skip the following checks if we are not currently in a const context.
            _ if self.const_kind.is_none() => {}

            hir::ExprKind::Loop(_, _, source, _) => {
                self.const_check_violated(NonConstExpr::Loop(*source), e.span);
            }

            hir::ExprKind::Match(_, _, source) => {
                let non_const_expr = match source {
                    // These are handled by `ExprKind::Loop` above.
                    hir::MatchSource::ForLoopDesugar => None,

                    _ => Some(NonConstExpr::Match(*source)),
                };

                if let Some(expr) = non_const_expr {
                    self.const_check_violated(expr, e.span);
                }
            }

            _ => {}
        }

        intravisit::walk_expr(self, e);
    }
}
