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
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Kotlin,
    Swift,
    Scala,
    Bash,
    Lua,
    Elixir,
}

impl Lang {
    /// Every supported language.
    pub const ALL: [Lang; 18] = [
        Lang::Rust,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::JavaScript,
        Lang::Python,
        Lang::Go,
        Lang::Java,
        Lang::C,
        Lang::Cpp,
        Lang::CSharp,
        Lang::Ruby,
        Lang::Php,
        Lang::Kotlin,
        Lang::Swift,
        Lang::Scala,
        Lang::Bash,
        Lang::Lua,
        Lang::Elixir,
    ];

    /// Detect the language from a path's extension (or, for a few, its file name).
    pub fn from_path(path: &str) -> Option<Lang> {
        let file = path.rsplit('/').next().unwrap_or(path);
        match file {
            "Rakefile" | "Gemfile" | "Guardfile" | "Vagrantfile" | "Podfile" => return Some(Lang::Ruby),
            ".bashrc" | ".bash_profile" | ".profile" => return Some(Lang::Bash),
            _ => {}
        }
        let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "java" => Lang::Java,
            "c" => Lang::C,
            // C++ parses C headers too; the C grammar can't parse C++ ones.
            "h" | "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "tpp" | "inl" => Lang::Cpp,
            "cs" => Lang::CSharp,
            "rb" | "rake" | "gemspec" | "ru" => Lang::Ruby,
            "php" | "phtml" => Lang::Php,
            "kt" | "kts" => Lang::Kotlin,
            "swift" => Lang::Swift,
            "scala" | "sc" | "sbt" => Lang::Scala,
            "sh" | "bash" => Lang::Bash,
            "lua" => Lang::Lua,
            "ex" | "exs" => Lang::Elixir,
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
            Lang::C => "C",
            Lang::Cpp => "C++",
            Lang::CSharp => "C#",
            Lang::Ruby => "Ruby",
            Lang::Php => "PHP",
            Lang::Kotlin => "Kotlin",
            Lang::Swift => "Swift",
            Lang::Scala => "Scala",
            Lang::Bash => "Bash",
            Lang::Lua => "Lua",
            Lang::Elixir => "Elixir",
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
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Lang::Swift => tree_sitter_swift::LANGUAGE.into(),
            Lang::Scala => tree_sitter_scala::LANGUAGE.into(),
            Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
            Lang::Lua => tree_sitter_lua::LANGUAGE.into(),
            Lang::Elixir => tree_sitter_elixir::LANGUAGE.into(),
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
    /// Does this item hold member items (a class's methods, a module's functions, ...)?
    pub has_members: bool,
}

impl Item {
    /// The identity used to match an item across versions.
    pub fn key(&self) -> ItemKey {
        ItemKey { kind: self.kind, container: self.container.clone(), name: self.name.clone(), ordinal: self.ordinal }
    }

    pub fn is_container(&self) -> bool {
        self.has_members
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

/// Leaf tokens of `node`, without comments and without the nodes covering `skip` ranges. The
/// tree's shape is included too (named node kinds as brackets): in indentation-sensitive
/// languages such as Python, re-indenting a line keeps the leaf tokens but changes the meaning.
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
    if node.is_named() {
        out.push(node.kind());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        tokens_except(child, source, skip, out);
    }
    if node.is_named() {
        out.push("\u{1}");
    }
}

/// Number of non-comment leaf tokens below `node`.
fn leaf_count(node: Node<'_>) -> usize {
    if node.kind().contains("comment") {
        return 0;
    }
    if node.child_count() == 0 {
        return 1;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).map(leaf_count).sum()
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
    let has_body = node.child_by_field_name("body").is_some();
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
        (Lang::C | Lang::Cpp, "function_definition") => {
            let (scope, name) = c_function_name(node, source)?;
            (if in_container || scope.is_some() { ItemKind::Method } else { ItemKind::Function }, name)
        }
        // A method declared (not defined) in a class body: `virtual double area() const = 0;`.
        (Lang::Cpp, "field_declaration") if in_container => (ItemKind::Method, c_function_name(node, source)?.1),
        (Lang::C | Lang::Cpp, "struct_specifier" | "union_specifier") if has_body => {
            (ItemKind::Struct, name_of("name")?)
        }
        (Lang::C | Lang::Cpp, "enum_specifier") if has_body => (ItemKind::Enum, name_of("name")?),
        (Lang::C | Lang::Cpp, "type_definition") => {
            let declarator = node.child_by_field_name("declarator").filter(|d| d.kind() == "type_identifier")?;
            (ItemKind::Type, text(declarator, source).to_owned())
        }
        (Lang::Cpp, "class_specifier") if has_body => (ItemKind::Class, name_of("name")?),
        (Lang::Cpp, "namespace_definition") => (ItemKind::Module, name_of("name")?),
        (Lang::Cpp, "alias_declaration") => (ItemKind::Type, name_of("name")?),
        (Lang::CSharp, "class_declaration" | "record_declaration") => (ItemKind::Class, name_of("name")?),
        (Lang::CSharp, "interface_declaration") => (ItemKind::Interface, name_of("name")?),
        (Lang::CSharp, "struct_declaration" | "record_struct_declaration") => (ItemKind::Struct, name_of("name")?),
        (Lang::CSharp, "enum_declaration") => (ItemKind::Enum, name_of("name")?),
        (Lang::CSharp, "delegate_declaration") => (ItemKind::Type, name_of("name")?),
        (Lang::CSharp, "method_declaration" | "constructor_declaration" | "destructor_declaration") => {
            (ItemKind::Method, name_of("name")?)
        }
        (Lang::CSharp, "local_function_statement") if !in_container => (ItemKind::Function, name_of("name")?),
        (Lang::CSharp, "namespace_declaration" | "file_scoped_namespace_declaration") => {
            (ItemKind::Module, name_of("name")?)
        }
        (Lang::Ruby, "class") => (ItemKind::Class, name_of("name")?),
        (Lang::Ruby, "module") => (ItemKind::Module, name_of("name")?),
        (Lang::Ruby, "method") => (if in_container { ItemKind::Method } else { ItemKind::Function }, name_of("name")?),
        (Lang::Ruby, "singleton_method") => (ItemKind::Method, name_of("name")?),
        (Lang::Php, "class_declaration") => (ItemKind::Class, name_of("name")?),
        (Lang::Php, "interface_declaration") => (ItemKind::Interface, name_of("name")?),
        (Lang::Php, "trait_declaration") => (ItemKind::Trait, name_of("name")?),
        (Lang::Php, "enum_declaration") => (ItemKind::Enum, name_of("name")?),
        (Lang::Php, "function_definition") => (ItemKind::Function, name_of("name")?),
        (Lang::Php, "method_declaration") => (ItemKind::Method, name_of("name")?),
        (Lang::Php, "namespace_definition") if has_body => (ItemKind::Module, name_of("name")?),
        (Lang::Kotlin, "class_declaration") => {
            let mut cursor = node.walk();
            let modifiers = node.named_children(&mut cursor).find(|c| c.kind() == "modifiers");
            let mut cursor = node.walk();
            let kind = if modifiers.is_some_and(|m| text(m, source).split_whitespace().any(|w| w == "enum")) {
                ItemKind::Enum
            } else if node.children(&mut cursor).any(|c| c.kind() == "interface") {
                ItemKind::Interface
            } else {
                ItemKind::Class
            };
            (kind, name_of("name")?)
        }
        (Lang::Kotlin, "object_declaration") => (ItemKind::Module, name_of("name")?),
        (Lang::Kotlin, "function_declaration") => {
            (if in_container { ItemKind::Method } else { ItemKind::Function }, name_of("name")?)
        }
        // tree-sitter-kotlin calls the alias's own name `type`.
        (Lang::Kotlin, "type_alias") => (ItemKind::Type, name_of("type")?),
        (Lang::Swift, "class_declaration") => {
            let kind = match node.child_by_field_name("declaration_kind").map(|k| text(k, source)) {
                Some("struct") => ItemKind::Struct,
                Some("enum") => ItemKind::Enum,
                Some("extension") => ItemKind::Impl,
                _ => ItemKind::Class,
            };
            (kind, name_of("name")?)
        }
        (Lang::Swift, "protocol_declaration") => (ItemKind::Interface, name_of("name")?),
        (Lang::Swift, "function_declaration" | "protocol_function_declaration") => {
            (if in_container { ItemKind::Method } else { ItemKind::Function }, name_of("name")?)
        }
        (Lang::Swift, "init_declaration") => (ItemKind::Method, "init".to_owned()),
        (Lang::Swift, "deinit_declaration") => (ItemKind::Method, "deinit".to_owned()),
        (Lang::Swift, "typealias_declaration") => (ItemKind::Type, name_of("name")?),
        (Lang::Scala, "class_definition") => (ItemKind::Class, name_of("name")?),
        (Lang::Scala, "object_definition") => (ItemKind::Module, name_of("name")?),
        (Lang::Scala, "trait_definition") => (ItemKind::Trait, name_of("name")?),
        (Lang::Scala, "enum_definition") => (ItemKind::Enum, name_of("name")?),
        (Lang::Scala, "function_definition" | "function_declaration") => {
            (if in_container { ItemKind::Method } else { ItemKind::Function }, name_of("name")?)
        }
        (Lang::Scala, "type_definition") => (ItemKind::Type, name_of("name")?),
        (Lang::Bash, "function_definition") => (ItemKind::Function, name_of("name")?),
        (Lang::Lua, "function_declaration") => {
            let name = node.child_by_field_name("name")?;
            match name.kind() {
                "dot_index_expression" => {
                    (ItemKind::Method, text(name.child_by_field_name("field")?, source).to_owned())
                }
                "method_index_expression" => {
                    (ItemKind::Method, text(name.child_by_field_name("method")?, source).to_owned())
                }
                _ => (ItemKind::Function, text(name, source).to_owned()),
            }
        }
        // `local f = function() … end`
        (Lang::Lua, "assignment_statement") if !in_container => {
            let mut cursor = node.walk();
            let vars = node.named_children(&mut cursor).find(|c| c.kind() == "variable_list")?;
            let mut cursor = node.walk();
            let values = node.named_children(&mut cursor).find(|c| c.kind() == "expression_list")?;
            values.named_child(0).filter(|v| v.kind() == "function_definition")?;
            (ItemKind::Function, text(vars, source).to_owned())
        }
        (Lang::Elixir, "call") => {
            let target = text(node.child_by_field_name("target")?, source);
            let mut cursor = node.walk();
            let first = node.named_children(&mut cursor).find(|c| c.kind() == "arguments")?.named_child(0)?;
            match target {
                "defmodule" | "defprotocol" | "defimpl" => (ItemKind::Module, text(first, source).to_owned()),
                "def" | "defp" | "defmacro" | "defmacrop" | "defguard" | "defguardp" | "defdelegate" => {
                    // `def f(x) when x > 0` → the head is the left of `when`.
                    let head =
                        if first.kind() == "binary_operator" { first.child_by_field_name("left")? } else { first };
                    let name = match head.kind() {
                        "call" => text(head.child_by_field_name("target")?, source),
                        _ => text(head, source),
                    };
                    (if in_container { ItemKind::Method } else { ItemKind::Function }, name.to_owned())
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(found)
}

/// The name a C/C++ function definition (or member declaration) declares, and its `Scope::`
/// qualifier for an out-of-class definition such as `double Shape::area() { … }`.
fn c_function_name(node: Node<'_>, source: &str) -> Option<(Option<String>, String)> {
    let mut declarator = node.child_by_field_name("declarator")?;
    // Through pointer and reference declarators (`int *f()`, `T& f()`) to the function one.
    for _ in 0..8 {
        if declarator.kind() == "function_declarator" {
            break;
        }
        declarator = declarator
            .child_by_field_name("declarator")
            .or_else(|| declarator.named_child(declarator.named_child_count().checked_sub(1)?))?;
    }
    if declarator.kind() != "function_declarator" {
        return None;
    }
    let mut inner = declarator.child_by_field_name("declarator")?;
    let mut scope = None;
    while inner.kind() == "qualified_identifier" {
        if let Some(s) = inner.child_by_field_name("scope") {
            scope = Some(text(s, source).to_owned());
        }
        inner = inner.child_by_field_name("name")?;
    }
    Some((scope, text(inner, source).to_owned()))
}

/// The table a Lua method belongs to (`function M.f()` / `function obj:m()`).
fn lua_table(node: Node<'_>, source: &str) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    matches!(name.kind(), "dot_index_expression" | "method_index_expression")
        .then(|| name.child_by_field_name("table").map(|t| text(t, source).to_owned()))?
}

/// The body of a definition: the `body` field, or the grammar's body node where it has no field.
fn body_of(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|c| matches!(c.kind(), "class_body" | "enum_class_body" | "function_body" | "do_block"))
    })
}

/// Do items of this kind hold member items (methods) in this language?
fn has_members(lang: Lang, kind: ItemKind) -> bool {
    match kind {
        ItemKind::Class | ItemKind::Impl | ItemKind::Trait | ItemKind::Interface | ItemKind::Module => true,
        ItemKind::Struct => matches!(lang, Lang::Cpp | Lang::CSharp | Lang::Swift),
        ItemKind::Enum => matches!(lang, Lang::Java | Lang::Swift | Lang::Kotlin | Lang::Php | Lang::Scala),
        _ => false,
    }
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
        // `export …`, decorators and C++ templates wrap the actual definition.
        let target = match child.kind() {
            "export_statement" | "decorated_definition" => child
                .child_by_field_name("declaration")
                .or_else(|| child.child_by_field_name("definition"))
                .unwrap_or(child),
            "template_declaration" => child.named_child(child.named_child_count().saturating_sub(1)).unwrap_or(child),
            _ => child,
        };
        if let Some((kind, name)) = classify(parsed.lang, target, source, in_container) {
            let range = child.byte_range(); // include `export`/decorators in the item's text
            let body = body_of(target);
            let sig_end = body.map(|b| b.start_byte()).unwrap_or(target.end_byte());
            let signature = squash(source.get(target.start_byte()..sig_end).unwrap_or(""));
            let mut toks = Vec::new();
            tokens(child, source, &mut toks);
            let token_hash = fnv(toks.iter());
            let anonymous_hash = fnv(toks.iter().map(|t| if *t == name { "\u{0}" } else { t }));
            let container_name = match (parsed.lang, kind) {
                (Lang::Go, ItemKind::Method) => go_receiver(target, source),
                (Lang::C | Lang::Cpp, ItemKind::Method) => c_function_name(target, source)
                    .and_then(|(scope, _)| scope)
                    .map(|s| s.rsplit("::").next().unwrap_or(&s).to_owned())
                    .or_else(|| container.map(str::to_owned)),
                (Lang::Lua, ItemKind::Method) => lua_table(target, source),
                _ => container.map(str::to_owned),
            };
            let members = has_members(parsed.lang, kind);
            let index = out.len();
            out.push(Item {
                end_line: child.end_position().row + 1,
                own_hash: token_hash,
                token_count: leaf_count(child),
                ordinal: 0,
                has_members: members,
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
            if members {
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
                    "type_declaration"
                        | "export_statement"
                        | "program"
                        | "source_file"
                        | "module"
                        | "declaration_list"
                        // C/C++ include guards and `extern "C" { … }`
                        | "preproc_if"
                        | "preproc_ifdef"
                        | "preproc_else"
                        | "preproc_elif"
                        | "preproc_elifdef"
                        | "linkage_specification"
                        // C# top-level statements, Lua `local …`
                        | "global_statement"
                        | "variable_declaration"
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

    fn has(lang: Lang, src: &str, expected: &[(&str, &str, bool)]) {
        let found = names(lang, src);
        for (name, kind, top) in expected {
            assert!(
                found.contains(&((*name).to_owned(), *kind, *top)),
                "{}: expected {kind} {name} (top={top}) in {found:?}",
                lang.name()
            );
        }
        let parsed = parse(lang, src).unwrap();
        assert!(!parsed.has_errors(), "{} sample should parse cleanly", lang.name());
    }

    #[test]
    fn c_and_cpp() {
        has(
            Lang::C,
            "#ifndef U_H\n#define U_H\nstruct point { int x; };\ntypedef struct point point_t;\nstatic int *helper(int a) { return 0; }\n#endif\n",
            &[("point", "struct", true), ("point_t", "type", true), ("helper", "function", true)],
        );
        has(
            Lang::Cpp,
            "namespace geo {\nclass Shape {\npublic:\n  virtual double area() const = 0;\n  void move(int dx) { x += dx; }\n  int x;\n};\n}\ndouble geo::Shape::perimeter() const { return 0; }\ntemplate <typename T> T max(T a, T b) { return a > b ? a : b; }\nusing Alias = int;\n",
            &[
                ("geo", "module", true),
                ("geo::Shape", "class", false),
                ("Shape::area", "method", false),
                ("Shape::move", "method", false),
                ("Shape::perimeter", "method", true),
                ("max", "function", true),
                ("Alias", "type", true),
            ],
        );
    }

    #[test]
    fn csharp_ruby_php() {
        has(
            Lang::CSharp,
            "namespace Acme {\n  public class Users : IService {\n    public Users() {}\n    public int Count(int a) { return a; }\n  }\n  interface IService { void Run(); }\n  struct P { void M() {} }\n  enum Kind { A }\n}\n",
            &[
                ("Acme", "module", true),
                ("Acme::Users", "class", false),
                ("Users::Users", "method", false),
                ("Users::Count", "method", false),
                ("IService::Run", "method", false),
                ("P::M", "method", false),
                ("Acme::Kind", "enum", false),
            ],
        );
        has(
            Lang::Ruby,
            "module Billing\n  class Invoice < Base\n    def total(a)\n      a\n    end\n    def self.build; end\n  end\nend\ndef helper; end\n",
            &[
                ("Billing", "module", true),
                ("Billing::Invoice", "class", false),
                ("Invoice::total", "method", false),
                ("Invoice::build", "method", false),
                ("helper", "function", true),
            ],
        );
        has(
            Lang::Php,
            "<?php\nnamespace App;\nclass UserController extends Controller {\n  public function show($id) { return $id; }\n}\ninterface Greeter { public function greet(); }\ntrait Loggable { function log() {} }\nfunction helper() {}\n",
            &[
                ("UserController", "class", true),
                ("UserController::show", "method", false),
                ("Greeter", "interface", true),
                ("Loggable", "trait", true),
                ("helper", "function", true),
            ],
        );
    }

    #[test]
    fn kotlin_swift_scala() {
        has(
            Lang::Kotlin,
            "interface Store {\n    fun get(): Int\n}\nclass Repo(val db: Db) {\n    fun find(id: Int): Int { return id }\n}\nobject Registry {\n    fun all(): Int { return 1 }\n}\nfun helper(a: Int): Int { return a }\nenum class Color { RED }\ntypealias Id = Int\n",
            &[
                ("Store", "interface", true),
                ("Store::get", "method", false),
                ("Repo::find", "method", false),
                ("Registry", "module", true),
                ("helper", "function", true),
                ("Color", "enum", true),
                ("Id", "type", true),
            ],
        );
        has(
            Lang::Swift,
            "class ViewController: UIViewController {\n    func load(id: Int) -> Int { return id }\n    init() {}\n}\nstruct Point { var x: Int\n  func len() -> Int { return x } }\nprotocol Drawable { func draw() }\nextension Point { func moved() -> Point { return self } }\nfunc helper() {}\n",
            &[
                ("ViewController::load", "method", false),
                ("ViewController::init", "method", false),
                ("Point", "struct", true),
                ("Point::len", "method", false),
                ("Drawable", "interface", true),
                ("Point", "impl", true),
                ("Point::moved", "method", false),
                ("helper", "function", true),
            ],
        );
        has(
            Lang::Scala,
            "class Repo(db: Db) {\n  def find(id: Int): Int = id\n}\nobject Repo { def apply(): Repo = ??? }\ntrait Store { def get(): Int }\ndef top(): Int = 1\n",
            &[
                ("Repo::find", "method", false),
                ("Repo", "module", true),
                ("Store::get", "method", false),
                ("top", "function", true),
            ],
        );
    }

    #[test]
    fn bash_lua_elixir() {
        has(
            Lang::Bash,
            "deploy() {\n  echo hi\n}\nfunction build {\n  helper\n}\n",
            &[("deploy", "function", true), ("build", "function", true)],
        );
        has(
            Lang::Lua,
            "local M = {}\nfunction M.helper(a) return a end\nlocal function private(b) return b end\nlocal f = function() return 1 end\nfunction obj:method() return 2 end\n",
            &[
                ("M::helper", "method", true),
                ("private", "function", true),
                ("f", "function", true),
                ("obj::method", "method", true),
            ],
        );
        has(
            Lang::Elixir,
            "defmodule Acme.Users do\n  def list(a), do: a\n  defp private(b) when b > 0 do\n    b\n  end\nend\n",
            &[
                ("Acme.Users", "module", true),
                ("Acme.Users::list", "method", false),
                ("Acme.Users::private", "method", false),
            ],
        );
    }

    #[test]
    fn languages_by_path() {
        assert_eq!(Lang::from_path("src/a.c"), Some(Lang::C));
        assert_eq!(Lang::from_path("include/a.h"), Some(Lang::Cpp));
        assert_eq!(Lang::from_path("App/Models/User.cs"), Some(Lang::CSharp));
        assert_eq!(Lang::from_path("Rakefile"), Some(Lang::Ruby));
        assert_eq!(Lang::from_path("lib/x.rb"), Some(Lang::Ruby));
        assert_eq!(Lang::from_path("build.gradle.kts"), Some(Lang::Kotlin));
        assert_eq!(Lang::from_path("scripts/deploy.sh"), Some(Lang::Bash));
        assert_eq!(Lang::from_path("mix.exs"), Some(Lang::Elixir));
        assert_eq!(Lang::from_path("README.md"), None);
        for lang in Lang::ALL {
            assert!(parse(lang, "").is_some(), "{} grammar loads", lang.name());
        }
    }

    #[test]
    fn a_changed_member_is_not_a_changed_container() {
        // C++/C#/Swift structs hold methods, so their own hash must leave members out.
        let before = &items(&parse(Lang::Cpp, "struct S {\n  int f() { return 1; }\n};\n").unwrap())[0];
        let after = &items(&parse(Lang::Cpp, "struct S {\n  int f() { return 2; }\n};\n").unwrap())[0];
        assert!(before.is_container());
        assert_eq!(before.own_hash, after.own_hash);
        assert_ne!(before.token_hash, after.token_hash);
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
