#![feature(rustc_private)]
#![warn(unused_extern_crates)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_lexer;
extern crate rustc_lint;
extern crate rustc_session;
extern crate rustc_span;

use std::ffi::OsStr;
use std::ops::Range;
use std::path::Path;

use dylint_linting::{declare_lint, declare_lint_pass, dylint_library, init_config};
use rustc_errors::DiagDecorator;
use rustc_hir::{AmbigArg, Expr, ExprKind, QPath, Ty, TyKind};
use rustc_lexer::{FrontmatterAllowed, TokenKind, strip_shebang, tokenize};
use rustc_lint::{LateContext, LateLintPass, LintContext, LintStore};
use rustc_session::Session;
use rustc_span::symbol::kw::{Crate as CRATE, SelfLower as SELF_LOWER, Super as SUPER};
use rustc_span::{BytePos, FileName, Span, Symbol};

dylint_library!();

declare_lint! {
    pub QUALIFIED_PATHS,
    Deny,
    "a path is qualified where it is used instead of imported"
}

declare_lint! {
    pub NO_SOURCE_COMMENTS,
    Deny,
    "Rust source contains a comment"
}

declare_lint_pass!(SourceStyle => [QUALIFIED_PATHS, NO_SOURCE_COMMENTS]);

#[unsafe(no_mangle)]
pub fn register_lints(session: &Session, store: &mut LintStore) {
    init_config(session);
    store.register_lints(&[QUALIFIED_PATHS, NO_SOURCE_COMMENTS]);
    store.register_late_lint_pass(Box::new(|_| Box::new(SourceStyle)));
}

impl<'tcx> LateLintPass<'tcx> for SourceStyle {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let files: Vec<_> = cx
            .sess()
            .source_map()
            .files()
            .iter()
            .filter(|file| {
                let FileName::Real(name) = &file.name else {
                    return false;
                };
                !file.is_imported()
                    && name.local_path().and_then(Path::extension) == Some(OsStr::new("rs"))
            })
            .cloned()
            .collect();
        for file in files {
            let Some(source) = &file.src else {
                continue;
            };
            for comment in comment_ranges(source) {
                let span = Span::with_root_ctxt(
                    file.start_pos + BytePos(comment.start),
                    file.start_pos + BytePos(comment.end),
                );
                cx.emit_span_lint(
                    NO_SOURCE_COMMENTS,
                    span,
                    DiagDecorator(|diag| {
                        diag.primary_message(
                            "rust source comments are not allowed; put documentation in Markdown",
                        );
                    }),
                );
            }
        }
    }

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

fn keyword(qpath: &QPath<'_>) -> Option<Symbol> {
    let QPath::Resolved(_, path) = qpath else {
        return None;
    };
    if path.segments.len() < 2 {
        return None;
    }
    let first = path.segments.first()?.ident;
    matches!(first.name, CRATE | SELF_LOWER | SUPER).then_some(first.name)
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

fn comment_ranges(source: &str) -> impl Iterator<Item = Range<u32>> {
    let start = strip_shebang(source).unwrap_or(0);
    let mut offset = start as u32;
    tokenize(&source[start..], FrontmatterAllowed::Yes).filter_map(move |token| {
        let start = offset;
        offset += token.len;
        match token.kind {
            TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } => Some(start..offset),
            _ => None,
        }
    })
}

#[test]
fn distinguishes_comments_from_rust_literals_and_shebangs() {
    let source = concat!(
        "#!/usr/bin/env rust-script\n",
        "//! docs 🛠\n",
        "#[cfg(any())]\n",
        "fn inactive() { /* ordinary /* nested */ block */ }\n",
        "/// item docs\n",
        "fn active() {\n",
        "    let _ = r#\"// raw string /* */\"#; // trailing\n",
        "    let _ = \"https://example.org\";\n",
        "    let _ = br\"/* raw bytes */\";\n",
        "    let _ = b'/';\n",
        "}\n",
    );
    let comments: Vec<_> = comment_ranges(source)
        .map(|range| &source[range.start as usize..range.end as usize])
        .collect();
    assert_eq!(
        comments,
        [
            "//! docs 🛠",
            "/* ordinary /* nested */ block */",
            "/// item docs",
            "// trailing"
        ],
    );
}
