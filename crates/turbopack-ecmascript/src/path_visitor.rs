use std::borrow::Cow;

use swc_core::{
    common::pass::AstKindPath,
    ecma::{
        ast::*,
        visit::{AstParentKind, VisitMut, VisitMutAstPath, VisitMutWith, VisitMutWithPath},
    },
};

use crate::code_gen::VisitorFactory;

pub type AstPath = Vec<AstParentKind>;

// Invariant: Each [AstPath] in `visitors` contains a value at position `index`.
pub struct ApplyVisitors<'a, 'b> {
    /// `VisitMut` should be shallow. In other words, it should not visit
    /// children of the node.
    visitors: Cow<'b, [(&'a AstPath, &'a dyn VisitorFactory)]>,

    index: usize,
}

/// Do two binary searches to find the sub-slice that has `path[index] == kind`.
/// Returns None if no item matches that. `visitors` need to be sorted by path.
fn find_range<'a, 'b>(
    visitors: &'b [(&'a AstPath, &'a dyn VisitorFactory)],
    kind: &AstParentKind,
    index: usize,
) -> Option<&'b [(&'a AstPath, &'a dyn VisitorFactory)]> {
    // Precondition: visitors is never empty
    let first_visitor = visitors.first().unwrap();

    if first_visitor.0[index] > *kind {
        // Fast path: If ast path of the first visitor is already out of range, then we
        // can skip the whole visit.
        return None;
    }

    let start = visitors.partition_point(|(path, _)| path[index] < *kind);
    if start >= visitors.len() {
        return None;
    }

    if visitors[start].0[index] > *kind {
        // Fast path: If the starting point is greater than the given kind, it's
        // meaningless to visit later.
        return None;
    }

    let end = if visitors.last().unwrap().0[index] == *kind {
        // Fast path: It's likely that the whole range is selected
        visitors.len()
    } else {
        visitors[start..].partition_point(|(path, _)| path[index] == *kind) + start
    };
    if end == start {
        return None;
    }
    // Postcondition: return value is never empty
    Some(&visitors[start..end])
}

impl<'a, 'b> ApplyVisitors<'a, 'b> {
    /// `visitors` must have an non-empty [AstPath].
    pub fn new(mut visitors: Vec<(&'a AstPath, &'a dyn VisitorFactory)>) -> Self {
        assert!(!visitors.is_empty());
        visitors.sort_by_key(|(path, _)| *path);
        Self {
            visitors: Cow::Owned(visitors),
            index: 0,
        }
    }

    #[inline(never)]
    fn visit_if_required<N>(&mut self, n: &mut N, ast_path: &mut AstKindPath<AstParentKind>)
    where
        N: for<'aa> VisitMutWith<dyn VisitMut + Send + Sync + 'aa>
            + for<'aa, 'bb> VisitMutWithPath<ApplyVisitors<'aa, 'bb>>,
    {
        let mut index = self.index;
        let mut current_visitors = self.visitors.as_ref();
        while index < ast_path.len() {
            let current = index == ast_path.len() - 1;
            let kind = ast_path[index];
            if let Some(visitors) = find_range(current_visitors, &kind, index) {
                // visitors contains all items that match kind at index. Some of them terminate
                // here, some need furth visiting. The terminating items are at the start due to
                // sorting of the list.
                index += 1;

                // skip items that terminate here
                let nested_visitors_start =
                    visitors.partition_point(|(path, _)| path.len() == index);
                if current {
                    // Potentially skip visiting this sub tree
                    if nested_visitors_start < visitors.len() {
                        n.visit_mut_children_with_path(
                            &mut ApplyVisitors {
                                // We only select visitors starting from `nested_visitors_start`
                                // which maintains the invariant.
                                visitors: Cow::Borrowed(&visitors[nested_visitors_start..]),
                                index,
                            },
                            ast_path,
                        );
                    }
                    for (_, visitor) in visitors[..nested_visitors_start].iter() {
                        n.visit_mut_with(&mut visitor.create());
                    }
                    return;
                } else {
                    current_visitors = &visitors[nested_visitors_start..];
                }
            } else {
                // Skip visiting this sub tree
                return;
            }
        }
        // Ast path is unchanged, just keep visiting
        n.visit_mut_children_with_path(self, ast_path);
    }
}

macro_rules! method {
    ($name:ident, $T:ty) => {
        fn $name(&mut self, n: &mut $T, ast_path: &mut AstKindPath<AstParentKind>) {
            self.visit_if_required(n, ast_path);
        }
    };
}

impl VisitMutAstPath for ApplyVisitors<'_, '_> {
    // TODO: we need a macro to apply that for all methods
    method!(visit_mut_prop, Prop);
    method!(visit_mut_expr, Expr);
    method!(visit_mut_pat, Pat);
    method!(visit_mut_stmt, Stmt);
    method!(visit_mut_module_decl, ModuleDecl);
    method!(visit_mut_module_item, ModuleItem);
    method!(visit_mut_call_expr, CallExpr);
    method!(visit_mut_lit, Lit);
    method!(visit_mut_str, Str);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use swc_core::{
        common::{errors::HANDLER, FileName, Mark, SourceFile, SourceMap},
        ecma::{
            ast::*,
            codegen::{text_writer::JsWriter, Emitter},
            parser::parse_file_as_module,
            transforms::base::resolver,
            visit::{fields::*, AstParentKind, VisitMut, VisitMutWith, VisitMutWithPath},
        },
        testing::run_test,
    };

    use super::{ApplyVisitors, VisitorFactory};

    fn parse(fm: &SourceFile) -> Module {
        let mut m = parse_file_as_module(
            fm,
            Default::default(),
            EsVersion::latest(),
            None,
            &mut vec![],
        )
        .map_err(|err| HANDLER.with(|handler| err.into_diagnostic(handler).emit()))
        .unwrap();

        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        m.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, false));

        m
    }

    struct StrReplacer<'a> {
        from: &'a str,
        to: &'a str,
    }

    impl VisitorFactory for Box<StrReplacer<'_>> {
        fn create<'a>(&'a self) -> Box<dyn VisitMut + Send + Sync + 'a> {
            box &**self
        }
    }

    impl VisitMut for &'_ StrReplacer<'_> {
        fn visit_mut_str(&mut self, s: &mut Str) {
            s.value = s.value.replace(self.from, self.to).into();
            s.raw = None;
        }
    }

    fn replacer(from: &'static str, to: &'static str) -> impl VisitorFactory {
        box StrReplacer { from, to }
    }

    fn to_js(m: &Module, cm: &Arc<SourceMap>) -> String {
        let mut bytes = Vec::new();
        let mut emitter = Emitter {
            cfg: swc_core::ecma::codegen::Config {
                minify: true,
                ..Default::default()
            },
            cm: cm.clone(),
            comments: None,
            wr: JsWriter::new(cm.clone(), "\n", &mut bytes, None),
        };

        emitter.emit_module(m).unwrap();

        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn path_visitor() {
        run_test(false, |cm, _handler| {
            let fm = cm.new_source_file(FileName::Anon, "('foo', 'bar', ['baz']);".into());

            let m = parse(&fm);

            let module_kind = AstParentKind::Module(ModuleField::Body(0));
            let module_item_kind = AstParentKind::ModuleItem(ModuleItemField::Stmt);
            let stmt_kind = AstParentKind::Stmt(StmtField::Expr);
            let expr_stmt_kind = AstParentKind::ExprStmt(ExprStmtField::Expr);
            let expr_kind = AstParentKind::Expr(ExprField::Paren);
            let paren_kind = AstParentKind::ParenExpr(ParenExprField::Expr);
            let expr2_kind = AstParentKind::Expr(ExprField::Seq);
            let seq_kind = AstParentKind::SeqExpr(SeqExprField::Exprs(1));
            let expr3_kind = AstParentKind::Expr(ExprField::Lit);
            let lit_kind = AstParentKind::Lit(LitField::Str);

            {
                let path = vec![
                    module_kind,
                    module_item_kind,
                    stmt_kind,
                    expr_stmt_kind,
                    expr_kind,
                    paren_kind,
                    expr2_kind,
                    seq_kind,
                    expr3_kind,
                    lit_kind,
                ];
                let bar_replacer = replacer("bar", "bar-success");

                let mut m = m.clone();
                m.visit_mut_with_path(
                    &mut ApplyVisitors::new(vec![(&path, &bar_replacer)]),
                    &mut Default::default(),
                );

                let s = to_js(&m, &cm);
                assert_eq!(s, r#"("foo","bar-success",["baz"]);"#);
            }

            {
                let wrong_path = vec![
                    module_kind,
                    module_item_kind,
                    stmt_kind,
                    expr_stmt_kind,
                    // expr_kind,
                    paren_kind,
                    expr2_kind,
                    seq_kind,
                    expr3_kind,
                    lit_kind,
                ];
                let bar_replacer = replacer("bar", "bar-success");

                let mut m = m.clone();
                m.visit_mut_with_path(
                    &mut ApplyVisitors::new(vec![(&wrong_path, &bar_replacer)]),
                    &mut Default::default(),
                );

                let s = to_js(&m, &cm);
                assert!(!s.contains("bar-success"));
            }

            drop(m);

            Ok(())
        })
        .unwrap();
    }
}
