//! Languages, parsing, and symbol ("item") extraction with Tree-sitter.

use std::ops::Range;

use tree_sitter::{Node, Parser, Tree};

/// A supported language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Java,
}

impl Lang {
    /// Detect the language from a path's extension.
    pub fn from_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "java" => Lang::Java,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::TypeScript => "TypeScript",
            Lang::Tsx => "TSX",
            Lang::JavaScript => "JavaScript",
            Lang::Python => "Python",
            Lang::Go => "Go",
            Lang::Java => "Java",
        }
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }
}

/// What kind of symbol an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ItemKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Impl,
    Type,
    Const,
    Module,
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Function => "function",
            ItemKind::Method => "method",
            ItemKind::Class => "class",
            ItemKind::Struct => "struct",
            ItemKind::Enum => "enum",
            ItemKind::Trait => "trait",
            ItemKind::Interface => "interface",
            ItemKind::Impl => "impl",
            ItemKind::Type => "type",
            ItemKind::Const => "constant",
            ItemKind::Module => "module",
        }
    }
}

/// One symbol in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: ItemKind,
    pub name: String,
    /// The enclosing class/impl/trait for methods.
    pub container: Option<String>,
    /// Byte range of the definition in the source.
    pub range: Range<usize>,
    /// 1-based first and last line of the definition.
    pub line: usize,
    pub end_line: usize,
    /// Is this a top-level definition (not nested in another item)?
    pub top_level: bool,
    /// Normalized text before the body (parameters, return type, …).
    pub signature: String,
    /// Hash of the exact text (formatting-sensitive).
    pub text_hash: u64,
    /// Hash of the tokens, ignoring whitespace and comments.
    pub token_hash: u64,
    /// Like `token_hash`, but with the item's own name masked (for rename detection).
    pub anonymous_hash: u64,
    /// For containers (classes, impls, …): the token hash without member items, so a changed
    /// method doesn't also count as a changed class. Equal to `token_hash` for other items.
    pub own_hash: u64,
    /// Number of tokens (renames are only inferred for non-trivial items).
    pub token_count: usize,
    /// Distinguishes items with the same kind, container and name (e.g. several `impl S`).
    pub ordinal: usize,
}

impl Item {
    /// The identity used to match an item across versions.
    pub fn key(&self) -> ItemKey {
        ItemKey { kind: self.kind, container: self.container.clone(), name: self.name.clone(), ordinal: self.ordinal }
    }

    pub fn is_container(&self) -> bool {
        matches!(self.kind, ItemKind::Class | ItemKind::Impl | ItemKind::Trait | ItemKind::Interface | ItemKind::Module)
    }

    pub fn display_name(&self) -> String {
        match &self.container {
            Some(c) => format!("{c}::{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// The identity of an item across versions of a file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemKey {
    pub kind: ItemKind,
    pub container: Option<String>,
    pub name: String,
    pub ordinal: usize,
}

/// A parsed source file.
pub struct Parsed {
    pub lang: Lang,
    pub tree: Tree,
    pub source: String,
}

impl Parsed {
    /// Did the parser have to recover from syntax errors?
    pub fn has_errors(&self) -> bool {
        self.tree.root_node().has_error()
    }
}

/// Parse `source` as `lang`.
pub fn parse(lang: Lang, source: &str) -> Option<Parsed> {
    let mut parser = Parser::new();
    parser.set_language(&lang.language()).ok()?;
    let tree = parser.parse(source, None)?;
    Some(Parsed { lang, tree, source: source.to_owned() })
}

fn fnv(parts: impl Iterator<Item = impl AsRef<[u8]>>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.as_ref() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash ^= 0x1f;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Leaf tokens of `node`, without comments.
fn tokens<'s>(node: Node<'_>, source: &'s str, out: &mut Vec<&'s str>) {
    tokens_except(node, source, &[], out);
}

/// Leaf tokens of `node`, without comments and without the nodes covering `skip` ranges.
fn tokens_except<'s>(node: Node<'_>, source: &'s str, skip: &[Range<usize>], out: &mut Vec<&'s str>) {
    if node.kind().contains("comment") || skip.iter().any(|r| *r == node.byte_range()) {
        return;
    }
    if node.child_count() == 0 {
        if let Some(text) = source.get(node.byte_range())
            && !text.trim().is_empty()
        {
            out.push(text);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        tokens_except(child, source, skip, out);
    }
}

/// Leaf tokens of the whole file outside the given (top-level item) ranges.
pub(crate) fn tokens_outside<'s>(parsed: &'s Parsed, skip: &[Range<usize>]) -> Vec<&'s str> {
    let mut out = Vec::new();
    tokens_except(parsed.tree.root_node(), &parsed.source, skip, &mut out);
    out
}

pub(crate) fn hash_tokens<'a>(toks: impl Iterator<Item = &'a str>) -> u64 {
    fnv(toks)
}

fn text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    source.get(node.byte_range()).unwrap_or("")
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Classify a node as a definition: (kind, name, is a container of methods).
fn classify(lang: Lang, node: Node<'_>, source: &str, in_container: bool) -> Option<(ItemKind, String)> {
    let name_of = |field: &str| node.child_by_field_name(field).map(|n| text(n, source).to_owned());
    let kind = node.kind();
    let found = match (lang, kind) {
        (Lang::Rust, "function_item") => {
            (if in_container { ItemKind::Method } else { ItemKind::Function }, name_of("name")?)
        }
        (Lang::Rust, "struct_item") => (ItemKind::Struct, name_of("name")?),
        (Lang::Rust, "enum_item") => (ItemKind::Enum, name_of("name")?),
        (Lang::Rust, "trait_item") => (ItemKind::Trait, name_of("name")?),
        (Lang::Rust, "type_item") => (ItemKind::Type, name_of("name")?),
        (Lang::Rust, "const_item" | "static_item") => (ItemKind::Const, name_of("name")?),
        (Lang::Rust, "mod_item") => (ItemKind::Module, name_of("name")?),
        (Lang::Rust, "macro_definition") => (ItemKind::Function, name_of("name")?),
        (Lang::Rust, "impl_item") => {
            let ty = name_of("type")?;
            let name = match name_of("trait") {
                Some(t) => format!("{t} for {ty}"),
                None => ty,
            };
            (ItemKind::Impl, name)
        }
        (
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript,
            "function_declaration" | "generator_function_declaration",
        ) => (ItemKind::Function, name_of("name")?),
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "class_declaration" | "abstract_class_declaration") => {
            (ItemKind::Class, name_of("name")?)
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "method_definition") => (ItemKind::Method, name_of("name")?),
        (Lang::TypeScript | Lang::Tsx, "interface_declaration") => (ItemKind::Interface, name_of("name")?),
        (Lang::TypeScript | Lang::Tsx, "type_alias_declaration") => (ItemKind::Type, name_of("name")?),
        (Lang::TypeScript | Lang::Tsx, "enum_declaration") => (ItemKind::Enum, name_of("name")?),
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "lexical_declaration" | "variable_declaration")
            if !in_container =>
        {
            // `const f = () => …` / `const f = function …`: a function in all but syntax.
            let mut cursor = node.walk();
            let declarator = node.named_children(&mut cursor).find(|c| c.kind() == "variable_declarator")?;
            let value = declarator.child_by_field_name("value")?;
            let name = declarator.child_by_field_name("name").map(|n| text(n, source).to_owned())?;
            match value.kind() {
                "arrow_function" | "function_expression" | "function" => (ItemKind::Function, name),
                _ => (ItemKind::Const, name),
            }
        }
        (Lang::Python, "function_definition") => {
            (if in_container { ItemKind::Method } else { ItemKind::Function }, name_of("name")?)
        }
        (Lang::Python, "class_definition") => (ItemKind::Class, name_of("name")?),
        (Lang::Go, "function_declaration") => (ItemKind::Function, name_of("name")?),
        (Lang::Go, "method_declaration") => (ItemKind::Method, name_of("name")?),
        (Lang::Go, "type_spec") => (ItemKind::Type, name_of("name")?),
        (Lang::Java, "class_declaration" | "record_declaration") => (ItemKind::Class, name_of("name")?),
        (Lang::Java, "interface_declaration") => (ItemKind::Interface, name_of("name")?),
        (Lang::Java, "enum_declaration") => (ItemKind::Enum, name_of("name")?),
        (Lang::Java, "method_declaration" | "constructor_declaration") => (ItemKind::Method, name_of("name")?),
        _ => return None,
    };
    Some(found)
}

/// For a Go method, the receiver type is the natural container.
fn go_receiver(node: Node<'_>, source: &str) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let t = text(receiver, source);
    let inner = t.trim_matches(|c| c == '(' || c == ')');
    inner.split_whitespace().last().map(|s| s.trim_start_matches('*').to_owned())
}

fn walk(parsed: &Parsed, node: Node<'_>, container: Option<&str>, top: bool, out: &mut Vec<Item>) {
    let source = parsed.source.as_str();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let in_container = container.is_some();
        // `export …` and decorators wrap the actual definition.
        let target = if matches!(child.kind(), "export_statement" | "decorated_definition") {
            child
                .child_by_field_name("declaration")
                .or_else(|| child.child_by_field_name("definition"))
                .unwrap_or(child)
        } else {
            child
        };
        if let Some((kind, name)) = classify(parsed.lang, target, source, in_container) {
            let range = child.byte_range(); // include `export`/decorators in the item's text
            let body = target.child_by_field_name("body");
            let sig_end = body.map(|b| b.start_byte()).unwrap_or(target.end_byte());
            let signature = squash(source.get(target.start_byte()..sig_end).unwrap_or(""));
            let mut toks = Vec::new();
            tokens(child, source, &mut toks);
            let token_hash = fnv(toks.iter());
            let anonymous_hash = fnv(toks.iter().map(|t| if *t == name { "\u{0}" } else { t }));
            let container_name = match (parsed.lang, kind) {
                (Lang::Go, ItemKind::Method) => go_receiver(target, source),
                _ => container.map(str::to_owned),
            };
            let index = out.len();
            out.push(Item {
                end_line: child.end_position().row + 1,
                own_hash: token_hash,
                token_count: toks.len(),
                ordinal: 0,
                kind,
                name: name.clone(),
                container: container_name,
                line: child.start_position().row + 1,
                top_level: top,
                signature,
                text_hash: fnv(std::iter::once(source.get(range.clone()).unwrap_or(""))),
                token_hash,
                anonymous_hash,
                range,
            });
            // Descend into classes, impls, traits, interfaces and modules for their members.
            if matches!(
                kind,
                ItemKind::Class | ItemKind::Impl | ItemKind::Trait | ItemKind::Interface | ItemKind::Module
            ) {
                let inner = body.unwrap_or(target);
                walk(parsed, inner, Some(&name), false, out);
                let members: Vec<Range<usize>> = out[index + 1..].iter().map(|i| i.range.clone()).collect();
                let mut own = Vec::new();
                tokens_except(child, source, &members, &mut own);
                out[index].own_hash = fnv(own.iter());
            }
            continue;
        }
        // Not a definition: look inside (e.g. Go `type_declaration` → `type_spec`, blocks).
        if child.named_child_count() > 0 {
            walk(
                parsed,
                child,
                container,
                top && matches!(
                    child.kind(),
                    "type_declaration" | "export_statement" | "program" | "source_file" | "module" | "declaration_list"
                ),
                out,
            );
        }
    }
}

/// All items in a parsed file, in source order.
pub fn items(parsed: &Parsed) -> Vec<Item> {
    let mut out = Vec::new();
    walk(parsed, parsed.tree.root_node(), None, true, &mut out);
    out.sort_by_key(|i| i.range.start);
    let mut seen: std::collections::HashMap<(ItemKind, Option<String>, String), usize> = Default::default();
    for item in &mut out {
        let n = seen.entry((item.kind, item.container.clone(), item.name.clone())).or_default();
        item.ordinal = *n;
        *n += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(lang: Lang, src: &str) -> Vec<(String, &'static str, bool)> {
        let p = parse(lang, src).unwrap();
        items(&p).into_iter().map(|i| (i.display_name(), i.kind.label(), i.top_level)).collect()
    }

    #[test]
    fn rust_items_and_methods() {
        let src = "struct S;\nimpl S {\n    fn new() -> S { S }\n}\nfn free(a: u32) -> u32 { a }\n";
        assert_eq!(
            names(Lang::Rust, src),
            vec![
                ("S".into(), "struct", true),
                ("S".into(), "impl", true),
                ("S::new".into(), "method", false),
                ("free".into(), "function", true)
            ]
        );
    }

    #[test]
    fn typescript_functions_classes_and_arrow_consts() {
        let src = "export function a() {}\nexport const b = () => 1;\nclass C { m() { return 1 } }\ninterface I { x: number }\n";
        let n = names(Lang::TypeScript, src);
        assert!(n.contains(&("a".into(), "function", true)), "{n:?}");
        assert!(n.contains(&("b".into(), "function", true)), "{n:?}");
        assert!(n.contains(&("C::m".into(), "method", false)), "{n:?}");
        assert!(n.contains(&("I".into(), "interface", true)), "{n:?}");
    }

    #[test]
    fn python_go_java() {
        assert!(names(Lang::Python, "class A:\n    def m(self):\n        pass\ndef f():\n    pass\n").contains(&(
            "A::m".into(),
            "method",
            false
        )));
        let go = names(Lang::Go, "package p\nfunc F() {}\ntype T struct{}\nfunc (t *T) M() {}\n");
        assert!(
            go.contains(&("T::M".into(), "method", false)) || go.contains(&("T::M".into(), "method", true)),
            "{go:?}"
        );
        assert!(go.contains(&("F".into(), "function", true)), "{go:?}");
        let java = names(Lang::Java, "class A { void m() {} }\n");
        assert!(java.contains(&("A::m".into(), "method", false)), "{java:?}");
    }

    #[test]
    fn token_hash_ignores_whitespace_and_comments_but_not_code() {
        let a = &items(&parse(Lang::Rust, "fn f(a: u32) -> u32 { a + 1 }").unwrap())[0];
        let b = &items(&parse(Lang::Rust, "fn f(a: u32) -> u32 {\n    // add one\n    a   +   1\n}").unwrap())[0];
        let c = &items(&parse(Lang::Rust, "fn f(a: u32) -> u32 { a + 2 }").unwrap())[0];
        assert_eq!(a.token_hash, b.token_hash);
        assert_ne!(a.text_hash, b.text_hash);
        assert_ne!(a.token_hash, c.token_hash);
    }
}
