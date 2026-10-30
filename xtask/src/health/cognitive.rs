//! Cognitive complexity of a function body, following the SonarSource
//! specification ("Cognitive Complexity", G. Ann Campbell, 2018) mapped onto
//! Rust syntax:
//!
//! - `if`, `match`, `for`, `while` and `loop` add 1 plus the current nesting
//!   level, and nest their bodies one level deeper. `if let`/`while let` are
//!   the same `if`/`while`; a `let ... else` is a branch like `if`.
//! - `else if` and `else` add 1 without a nesting penalty (hybrid increment).
//! - Every run of like logical operators (`&&` or `||`) adds 1; switching
//!   operator starts a new run, so `a && b || c` adds 2.
//! - A labelled `break`/`continue` and a direct recursive call each add 1.
//! - Closures and async blocks add nothing but nest their bodies.
//!
//! The `?` operator adds nothing: it is Rust's idiomatic early return, which
//! the specification exempts. Functions declared inside a body are separate
//! functions and are measured on their own. Macro arguments are scored when
//! they parse as expressions (`println!`, `vec!`, `assert!`), since code
//! hidden in them is still code a reader has to follow.

use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{BinOp, Block, Expr, Token};

/// What reading one function body costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BodyMetrics {
    /// SonarSource cognitive complexity.
    pub(crate) complexity: u32,
    /// Deepest nesting level reached, counting every structure that nests
    /// (conditionals, loops, `match`, closures).
    pub(crate) nesting: u32,
}

/// Scores the body of the function named `fn_name`; the name is needed to
/// recognise recursive calls.
pub(crate) fn measure(fn_name: &str, body: &Block) -> BodyMetrics {
    let mut scorer = Scorer {
        fn_name,
        level: 0,
        metrics: BodyMetrics {
            complexity: 0,
            nesting: 0,
        },
    };
    scorer.visit_block(body);
    scorer.metrics
}

struct Scorer<'n> {
    fn_name: &'n str,
    level: u32,
    metrics: BodyMetrics,
}

impl Scorer<'_> {
    /// A structural increment: 1 plus the nesting penalty.
    fn structural(&mut self) {
        self.metrics.complexity += 1 + self.level;
    }

    fn nested(&mut self, walk: impl FnOnce(&mut Self)) {
        self.level += 1;
        self.metrics.nesting = self.metrics.nesting.max(self.level);
        walk(self);
        self.level -= 1;
    }

    /// The `else` side of an `if`: `else if` continues the chain at the same
    /// level, a plain `else` is one more nested block.
    fn visit_else(&mut self, else_branch: &Expr) {
        self.metrics.complexity += 1;
        match else_branch {
            Expr::If(chained) => {
                self.visit_expr(&chained.cond);
                self.nested(|s| s.visit_block(&chained.then_branch));
                if let Some((_, next)) = &chained.else_branch {
                    self.visit_else(next);
                }
            }
            other => self.nested(|s| s.visit_expr(other)),
        }
    }

    fn is_self_call(&self, func: &Expr) -> bool {
        let Expr::Path(path) = func else {
            return false;
        };
        let segments = &path.path.segments;
        let named_here = segments.last().is_some_and(|s| s.ident == self.fn_name);
        let qualifier_is_self = segments.len() == 1 || segments[0].ident == "Self";
        named_here && qualifier_is_self
    }
}

impl<'ast> Visit<'ast> for Scorer<'_> {
    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.structural();
        self.visit_expr(&node.cond);
        self.nested(|s| s.visit_block(&node.then_branch));
        if let Some((_, else_branch)) = &node.else_branch {
            self.visit_else(else_branch);
        }
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        self.structural();
        self.visit_expr(&node.expr);
        self.nested(|s| node.arms.iter().for_each(|arm| s.visit_arm(arm)));
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.structural();
        self.visit_expr(&node.expr);
        self.nested(|s| s.visit_block(&node.body));
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.structural();
        self.visit_expr(&node.cond);
        self.nested(|s| s.visit_block(&node.body));
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.structural();
        self.nested(|s| s.visit_block(&node.body));
    }

    fn visit_local_init(&mut self, node: &'ast syn::LocalInit) {
        self.visit_expr(&node.expr);
        if let Some((_, diverge)) = &node.diverge {
            self.structural();
            self.nested(|s| s.visit_expr(diverge));
        }
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.nested(|s| s.visit_expr(&node.body));
    }

    fn visit_expr_async(&mut self, node: &'ast syn::ExprAsync) {
        self.nested(|s| s.visit_block(&node.block));
    }

    fn visit_expr_break(&mut self, node: &'ast syn::ExprBreak) {
        self.metrics.complexity += u32::from(node.label.is_some());
        visit::visit_expr_break(self, node);
    }

    fn visit_expr_continue(&mut self, node: &'ast syn::ExprContinue) {
        self.metrics.complexity += u32::from(node.label.is_some());
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if logical_op(&node.op).is_none() {
            return visit::visit_expr_binary(self, node);
        }
        let mut ops = Vec::new();
        let mut operands = Vec::new();
        flatten_logical(&node.left, &mut ops, &mut operands);
        ops.extend(logical_op(&node.op));
        flatten_logical(&node.right, &mut ops, &mut operands);
        self.metrics.complexity += count_runs(&ops);
        for operand in operands {
            self.visit_expr(operand);
        }
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        self.metrics.complexity += u32::from(self.is_self_call(&node.func));
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let on_self = matches!(&*node.receiver, Expr::Path(p) if p.path.is_ident("self"));
        self.metrics.complexity += u32::from(on_self && node.method == self.fn_name);
        visit::visit_expr_method_call(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let parser = Punctuated::<Expr, Token![,]>::parse_terminated;
        if let Ok(args) = node.parse_body_with(parser) {
            args.iter().for_each(|e| self.visit_expr(e));
        }
    }

    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

/// `Some(true)` for `&&`, `Some(false)` for `||`, `None` otherwise.
fn logical_op(op: &BinOp) -> Option<bool> {
    match op {
        BinOp::And(_) => Some(true),
        BinOp::Or(_) => Some(false),
        _ => None,
    }
}

/// Collects, left to right, the logical operators of the expression tree
/// rooted at `expr` (looking through parentheses) and the operands they join.
fn flatten_logical<'a>(expr: &'a Expr, ops: &mut Vec<bool>, operands: &mut Vec<&'a Expr>) {
    match expr {
        Expr::Binary(bin) if logical_op(&bin.op).is_some() => {
            flatten_logical(&bin.left, ops, operands);
            ops.extend(logical_op(&bin.op));
            flatten_logical(&bin.right, ops, operands);
        }
        Expr::Paren(paren) if matches!(&*paren.expr, Expr::Binary(b) if logical_op(&b.op).is_some()) =>
        {
            flatten_logical(&paren.expr, ops, operands);
        }
        other => operands.push(other),
    }
}

/// Number of runs of equal operators: `[&&, &&, ||, &&]` has 3.
fn count_runs(ops: &[bool]) -> u32 {
    let changes = ops.windows(2).filter(|pair| pair[0] != pair[1]).count();
    let runs = if ops.is_empty() { 0 } else { changes + 1 };
    u32::try_from(runs).expect("operator count fits in u32")
}

#[cfg(test)]
mod tests {
    use super::{BodyMetrics, measure};

    fn score(src: &str) -> BodyMetrics {
        let item: syn::ItemFn = syn::parse_str(src).expect("test source parses");
        measure(&item.sig.ident.to_string(), &item.block)
    }

    fn complexity(src: &str) -> u32 {
        score(src).complexity
    }

    #[test]
    fn straight_line_code_is_free() {
        assert_eq!(
            complexity("fn f(x: u32) -> u32 { let y = x + 1; y? ; y * 2 }"),
            0
        );
    }

    #[test]
    fn sonar_paper_sum_of_primes_scores_7() {
        // The worked example from the SonarSource white paper.
        let src = "fn sum_of_primes(max: u32) -> u32 {
            let mut total = 0;
            'out: for i in 1..=max {
                for j in 2..i {
                    if i % j == 0 {
                        continue 'out;
                    }
                }
                total += i;
            }
            total
        }";
        assert_eq!(complexity(src), 7);
    }

    #[test]
    fn match_counts_once_not_per_arm() {
        let src = "fn words(n: u32) -> &'static str {
            match n { 1 => \"one\", 2 => \"a couple\", 3 => \"a few\", _ => \"lots\" }
        }";
        assert_eq!(complexity(src), 1);
    }

    #[test]
    fn else_if_and_else_are_hybrid_increments() {
        let src = "fn f(a: bool, b: bool) {
            if a { g(); } else if b { h(); } else { i(); }
        }";
        assert_eq!(complexity(src), 3);
    }

    #[test]
    fn nesting_penalty_grows_with_depth() {
        // if (+1), nested for (+2), nested while (+3).
        let src = "fn f(a: bool, v: &[u32]) {
            if a { for x in v { while *x > 0 { g(); } } }
        }";
        assert_eq!(
            score(src),
            BodyMetrics {
                complexity: 6,
                nesting: 3
            }
        );
    }

    #[test]
    fn each_run_of_like_logical_operators_adds_one() {
        assert_eq!(
            complexity("fn f(a: bool, b: bool, c: bool) -> bool { a && b && c }"),
            1
        );
        assert_eq!(
            complexity("fn f(a: bool, b: bool, c: bool) -> bool { a && b || c }"),
            2
        );
        assert_eq!(
            complexity("fn f(a: bool, b: bool, c: bool, d: bool) -> bool { a && (b || c) && d }"),
            3
        );
        assert_eq!(
            complexity("fn f(a: bool, b: bool) -> bool { if a || b { true } else { false } }"),
            3
        );
    }

    #[test]
    fn closures_nest_without_incrementing() {
        let src = "fn f(v: &[u32]) -> Vec<u32> { v.iter().map(|x| if *x > 1 { 1 } else { 0 }).collect() }";
        assert_eq!(
            score(src),
            BodyMetrics {
                complexity: 3,
                nesting: 2
            }
        );
    }

    #[test]
    fn recursion_adds_one() {
        let src = "fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }";
        assert_eq!(complexity(src), 3);
    }

    #[test]
    fn let_else_is_a_branch() {
        let src = "fn f(x: Option<u32>) -> u32 { let Some(y) = x else { return 0 }; y }";
        assert_eq!(complexity(src), 1);
    }

    #[test]
    fn macro_arguments_are_scored() {
        let src = "fn f(a: bool) { println!(\"{}\", if a { 1 } else { 2 }); }";
        assert_eq!(complexity(src), 2);
    }

    #[test]
    fn nested_fn_items_are_measured_separately() {
        let src = "fn outer() { fn inner(a: bool) { if a { g(); } } inner(true); }";
        assert_eq!(complexity(src), 0);
    }
}
