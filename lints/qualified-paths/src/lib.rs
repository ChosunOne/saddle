#![feature(rustc_private)]
#![warn(unused_extern_crates)]

//! D148: `crate::`, `self::` and `super::` written where a path is used — in an expression, a type
//! or a pattern — which no rustc or Clippy lint sees. `absolute_paths` allows them because they
//! start with a keyword, and `items_after_statements` only looks after statements. A module's
//! imports are at the top of the module, so a name used below is spelled plainly.

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use dylint_linting::declare_late_lint;
use rustc_errors::DiagDecorator;
use rustc_hir::{AmbigArg, Expr, ExprKind, QPath, Ty, TyKind};
use rustc_lint::LintContext;
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::Symbol;

declare_late_lint! {
    /// ### What it does
    ///
    /// Reports a `crate::`, `self::` or `super::` path written where it is used, in an expression,
    /// a type or a pattern, instead of at the top of the module with the other imports.
    ///
    /// ### Why is this bad?
    ///
    /// A qualified path says where a name lives at the point of use, which is the second place to
    /// look when it changes. It also makes a module's real dependencies invisible at its head, and
    /// it accumulates: twenty-eight of them survived a sweep that removed a thousand, because no
    /// lint was watching.
    ///
    /// ### Known problems
    ///
    /// A path in a macro expansion is reported at the expansion's own span, so a `crate::` path
    /// inside a `macro_rules!` definition is reported once per use.
    ///
    /// ### Example
    ///
    /// ```rust
    /// fn purpose() -> Purpose {
    ///     crate::tui::login::Purpose::Model
    /// }
    /// ```
    ///
    /// Use instead:
    ///
    /// ```rust
    /// use crate::tui::login::Purpose;
    ///
    /// fn purpose() -> Purpose {
    ///     Purpose::Model
    /// }
    /// ```
    pub QUALIFIED_PATHS,
    Deny,
    "a path is qualified where it is used instead of imported"
}

impl<'tcx> LateLintPass<'tcx> for QualifiedPaths {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>) {
        match &expr.kind {
            ExprKind::Path(qpath) => check(cx, qpath),
            ExprKind::Struct(qpath, _, _) => check(cx, qpath),
            _ => {}
        }
    }

    fn check_ty(&mut self, cx: &LateContext<'tcx>, ty: &'tcx Ty<'tcx, AmbigArg>) {
        if let TyKind::Path(qpath) = &ty.kind {
            check(cx, qpath);
        }
    }
}

/// The keyword a path starts with, when it is one of the three that mean "in this crate".
fn keyword(qpath: &QPath<'_>) -> Option<Symbol> {
    let QPath::Resolved(_, path) = qpath else {
        return None;
    };
    // A bare `self` is a receiver, not a qualified path; `self::x` and `crate::x` have a second
    // segment and are what this looks for.
    if path.segments.len() < 2 {
        return None;
    }
    let first = path.segments.first()?.ident;
    [
        rustc_span::symbol::kw::Crate,
        rustc_span::symbol::kw::SelfLower,
        rustc_span::symbol::kw::Super,
    ]
    .contains(&first.name)
    .then_some(first.name)
}

fn check(cx: &LateContext<'_>, qpath: &QPath<'_>) {
    let Some(keyword) = keyword(qpath) else {
        return;
    };
    let written = qpath.span();
    cx.emit_span_lint(
        QUALIFIED_PATHS,
        written,
        DiagDecorator(|diag| {
            diag.primary_message(format!(
                "`{keyword}::` is written where the path is used; import the name at the top of \
                 the module instead"
            ));
        }),
    );
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
