//! Finds every function with a body in one source file and measures it.

use syn::visit::{self, Visit};
use syn::{Block, Signature, Type};

use super::cognitive::{self, BodyMetrics};

/// One measured function.
#[derive(Debug)]
pub(crate) struct Function {
    /// Path inside the file, such as `tests::parses` or `<Config as Default>::default`.
    pub(crate) name: String,
    /// Lines from the `fn` keyword to the closing brace.
    pub(crate) lines: u32,
    pub(crate) body: BodyMetrics,
}

/// Every function, method and default trait method in `file`, nested ones
/// included.
pub(crate) fn collect(file: &syn::File) -> Vec<Function> {
    let mut collector = Collector::default();
    collector.visit_file(file);
    collector.functions
}

#[derive(Default)]
struct Collector {
    scope: Vec<String>,
    functions: Vec<Function>,
}

impl Collector {
    fn record(&mut self, sig: &Signature, block: &Block) {
        let fn_name = sig.ident.to_string();
        let first = sig.fn_token.span.start().line;
        let last = block.brace_token.span.close().end().line;
        let name = self
            .scope
            .iter()
            .chain(std::iter::once(&fn_name))
            .cloned()
            .collect::<Vec<_>>()
            .join("::");
        self.functions.push(Function {
            name,
            lines: u32::try_from(last - first + 1).unwrap_or(u32::MAX),
            body: cognitive::measure(&fn_name, block),
        });
    }

    /// Records the function, then looks for functions declared inside it.
    fn function(&mut self, sig: &Signature, block: &Block) {
        self.record(sig, block);
        self.scoped(sig.ident.to_string(), |c| c.visit_block(block));
    }

    fn scoped(&mut self, segment: String, walk: impl FnOnce(&mut Self)) {
        self.scope.push(segment);
        walk(self);
        self.scope.pop();
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.function(&node.sig, &node.block);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.function(&node.sig, &node.block);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if let Some(block) = &node.default {
            self.function(&node.sig, block);
        }
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.scoped(node.ident.to_string(), |c| visit::visit_item_mod(c, node));
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        self.scoped(node.ident.to_string(), |c| visit::visit_item_trait(c, node));
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let self_ty = type_name(&node.self_ty);
        let segment = match &node.trait_ {
            Some((path, _)) => format!("<{self_ty} as {}>", last_ident(path)),
            None => self_ty,
        };
        self.scoped(segment, |c| visit::visit_item_impl(c, node));
    }
}

fn type_name(ty: &Type) -> String {
    match ty {
        Type::Path(path) => last_ident(&path.path),
        Type::Reference(reference) => type_name(&reference.elem),
        _ => "_".to_owned(),
    }
}

fn last_ident(path: &syn::Path) -> String {
    path.segments
        .last()
        .map_or_else(|| "_".to_owned(), |s| s.ident.to_string())
}

#[cfg(test)]
mod tests {
    use super::collect;

    #[test]
    fn names_functions_by_their_path_in_the_file() {
        let file = syn::parse_file(
            "fn top() {}
             struct S;
             impl S { fn method(&self) {} }
             impl Default for S {
                 fn default() -> Self {
                     fn helper() {}
                     S
                 }
             }
             trait T { fn provided(&self) { if true {} } fn required(&self); }
             mod tests { fn case() {} }",
        )
        .unwrap();
        let names: Vec<_> = collect(&file).into_iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            [
                "top",
                "S::method",
                "<S as Default>::default",
                "<S as Default>::default::helper",
                "T::provided",
                "tests::case",
            ]
        );
    }

    #[test]
    fn counts_lines_from_fn_keyword_to_closing_brace() {
        let file = syn::parse_file("/// doc\n#[inline]\nfn f() {\n    g();\n}\n").unwrap();
        assert_eq!(collect(&file)[0].lines, 3);
    }
}
