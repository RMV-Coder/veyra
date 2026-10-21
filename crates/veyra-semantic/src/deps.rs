//! Dependency extraction for impact analysis: what a file imports and which names it uses.

use std::collections::BTreeSet;

use tree_sitter::Node;

use crate::items::{Lang, Parsed};

/// One import / use / require.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The module path as written (`./util`, `crate::merge`, `os.path`, `fmt`, `java.util.List`).
    pub module: String,
    /// Imported names, when the syntax lists them (`{a, b}`, `from x import a, b`, `use x::{a, b}`).
    pub names: Vec<String>,
    pub line: usize,
}

fn text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    source.get(node.byte_range()).unwrap_or("")
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`').to_owned()
}

fn named<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// Identifiers anywhere below `node`.
fn idents_below(node: Node<'_>, source: &str, out: &mut Vec<String>) {
    if node.child_count() == 0 {
        // PHP calls identifiers `name`, Ruby has `constant`s, Elixir `alias`es, Bash `word`s.
        if node.kind().contains("identifier") || matches!(node.kind(), "name" | "constant" | "alias" | "word") {
            out.push(text(node, source).to_owned());
        }
        return;
    }
    for child in named(node) {
        idents_below(child, source, out);
    }
}

fn rust_use(arg: &str) -> (String, Vec<String>) {
    // `a::b::{c, d}` → (a::b, [c, d]); `a::b::c` → (a::b, [c]); `a::b as x` → (a::b, [x])
    let arg = arg.trim().trim_end_matches(';');
    if let Some((module, list)) = arg.split_once("::{") {
        let names =
            list.trim_end_matches('}').split(',').map(|n| n.trim().rsplit("::").next().unwrap_or("").to_owned());
        return (module.to_owned(), names.filter(|n| !n.is_empty() && n != "self").collect());
    }
    let (path, alias) = match arg.split_once(" as ") {
        Some((p, a)) => (p.trim(), Some(a.trim())),
        None => (arg, None),
    };
    match path.rsplit_once("::") {
        Some((module, last)) => (module.to_owned(), vec![alias.unwrap_or(last).to_owned()]),
        None => (path.to_owned(), Vec::new()),
    }
}

fn collect(lang: Lang, node: Node<'_>, source: &str, out: &mut Vec<Import>) {
    let line = node.start_position().row + 1;
    let field = |n: Node<'_>, f: &str| n.child_by_field_name(f).map(|c| text(c, source).to_owned());
    let mut push = |module: String, names: Vec<String>| {
        if !module.is_empty() {
            out.push(Import { module, names, line });
        }
    };
    match (lang, node.kind()) {
        (Lang::Rust, "use_declaration") => {
            if let Some(arg) = field(node, "argument") {
                let (module, names) = rust_use(&arg);
                push(module, names);
            }
            return;
        }
        (Lang::Rust, "mod_item") if node.child_by_field_name("body").is_none() => {
            if let Some(name) = field(node, "name") {
                push(format!("self::{name}"), Vec::new());
            }
            return;
        }
        (Lang::Rust, "extern_crate_declaration") => {
            if let Some(name) = field(node, "name") {
                push(name, Vec::new());
            }
            return;
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "import_statement" | "export_statement") => {
            if let Some(src) = field(node, "source") {
                let mut names = Vec::new();
                for child in named(node) {
                    if matches!(child.kind(), "import_clause" | "export_clause") {
                        idents_below(child, source, &mut names);
                    }
                }
                push(unquote(&src), names);
                return;
            }
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "call_expression") => {
            let callee = field(node, "function").unwrap_or_default();
            if (callee == "require" || callee == "import")
                && let Some(args) = node.child_by_field_name("arguments")
                && let Some(first) = named(args).into_iter().next().filter(|a| a.kind() == "string")
            {
                push(unquote(text(first, source)), Vec::new());
            }
        }
        (Lang::Python, "import_statement") => {
            for child in named(node) {
                let module = match child.kind() {
                    "aliased_import" => field(child, "name"),
                    _ => Some(text(child, source).to_owned()),
                };
                push(module.unwrap_or_default(), Vec::new());
            }
            return;
        }
        (Lang::Python, "import_from_statement") => {
            let module = field(node, "module_name").unwrap_or_default();
            let mut names = Vec::new();
            let mut cursor = node.walk();
            for child in node.children_by_field_name("name", &mut cursor) {
                let n = match child.kind() {
                    "aliased_import" => child.child_by_field_name("name").map(|c| text(c, source).to_owned()),
                    _ => Some(text(child, source).to_owned()),
                };
                names.extend(n);
            }
            push(module, names);
            return;
        }
        (Lang::Go, "import_spec") => {
            if let Some(path) = field(node, "path") {
                push(unquote(&path), Vec::new());
            }
            return;
        }
        (Lang::Java, "import_declaration") => {
            let path = text(node, source).trim_start_matches("import").trim().trim_end_matches(';').trim();
            let path = path.strip_prefix("static ").unwrap_or(path).trim();
            match path.rsplit_once('.') {
                Some((module, last)) if last != "*" => push(module.to_owned(), vec![last.to_owned()]),
                Some((module, _)) => push(module.to_owned(), Vec::new()),
                None => push(path.to_owned(), Vec::new()),
            }
            return;
        }
        (Lang::C | Lang::Cpp, "preproc_include") => {
            if let Some(path) = field(node, "path") {
                push(path.trim().trim_matches(|c| c == '"' || c == '<' || c == '>').to_owned(), Vec::new());
            }
            return;
        }
        (Lang::CSharp, "using_directive") => {
            // `using A.B;`, `global using A.B;`, `using static A.B;`, `using X = A.B;`
            let t = text(node, source).trim().trim_end_matches(';').trim();
            let t = t.strip_prefix("global").map(str::trim_start).unwrap_or(t);
            let t = t.strip_prefix("using").map(str::trim_start).unwrap_or(t);
            let t = t.strip_prefix("static ").unwrap_or(t);
            push(t.split_once('=').map(|(_, m)| m).unwrap_or(t).trim().to_owned(), Vec::new());
            return;
        }
        (Lang::Ruby, "call") => {
            let method = field(node, "method").unwrap_or_default();
            if matches!(method.as_str(), "require" | "require_relative" | "load")
                && node.child_by_field_name("receiver").is_none()
                && let Some(args) = node.child_by_field_name("arguments")
                && let Some(first) = named(args).into_iter().next().filter(|a| a.kind() == "string")
            {
                let path = unquote(text(first, source));
                let relative = method == "require_relative" && !path.starts_with('.');
                push(if relative { format!("./{path}") } else { path }, Vec::new());
                return;
            }
        }
        (Lang::Php, "namespace_use_declaration") => {
            // `use A\B\C;`, `use A\B\C as D;`, `use A\B\{C, D};`
            let group = node.child_by_field_name("body");
            let prefix = named(node).into_iter().find(|c| c.kind() == "namespace_name").map(|n| text(n, source));
            let clauses = group.map(named).unwrap_or_else(|| named(node));
            for clause in clauses.into_iter().filter(|c| c.kind() == "namespace_use_clause") {
                let Some(name) = named(clause).into_iter().find(|c| matches!(c.kind(), "qualified_name" | "name"))
                else {
                    continue;
                };
                let full = match prefix.filter(|_| group.is_some()) {
                    Some(prefix) => format!("{prefix}\\{}", text(name, source)),
                    None => text(name, source).to_owned(),
                };
                let full = full.trim_start_matches('\\');
                match full.rsplit_once('\\') {
                    Some((module, last)) => push(module.to_owned(), vec![last.to_owned()]),
                    None => push(full.to_owned(), Vec::new()),
                }
            }
            return;
        }
        (
            Lang::Php,
            "require_expression" | "require_once_expression" | "include_expression" | "include_once_expression",
        ) => {
            if let Some(s) = named(node).into_iter().find(|c| matches!(c.kind(), "string" | "encapsed_string")) {
                push(unquote(text(s, source)), Vec::new());
            }
            return;
        }
        (Lang::Kotlin, "import") => {
            let path = text(node, source).trim().trim_start_matches("import").trim().trim_end_matches(';');
            let path = path.split(" as ").next().unwrap_or(path).trim();
            match path.strip_suffix(".*") {
                Some(module) => push(module.to_owned(), Vec::new()),
                None => match path.rsplit_once('.') {
                    Some((module, last)) => push(module.to_owned(), vec![last.to_owned()]),
                    None => push(path.to_owned(), Vec::new()),
                },
            }
            return;
        }
        (Lang::Swift, "import_declaration") => {
            if let Some(id) = named(node).into_iter().find(|c| c.kind() == "identifier") {
                push(text(id, source).to_owned(), Vec::new());
            }
            return;
        }
        (Lang::Scala, "import_declaration") => {
            let mut cursor = node.walk();
            let path: Vec<&str> = node
                .children_by_field_name("path", &mut cursor)
                .filter(|p| p.is_named())
                .map(|p| text(p, source))
                .collect();
            let selectors = named(node).into_iter().find(|c| c.kind() == "namespace_selectors");
            let wildcard = named(node).into_iter().any(|c| c.kind() == "namespace_wildcard");
            match (selectors, path.split_last()) {
                (Some(selectors), _) => {
                    let mut names = Vec::new();
                    idents_below(selectors, source, &mut names);
                    push(path.join("."), names);
                }
                (None, Some((last, init))) if !wildcard && !init.is_empty() => {
                    push(init.join("."), vec![(*last).to_owned()]);
                }
                _ => push(path.join("."), Vec::new()),
            }
            return;
        }
        (Lang::Bash, "command") => {
            let name = field(node, "name").unwrap_or_default();
            if (name == "source" || name == ".")
                && let Some(arg) = node.child_by_field_name("argument")
            {
                push(unquote(text(arg, source)), Vec::new());
                return;
            }
        }
        (Lang::Lua, "function_call") => {
            if field(node, "name").as_deref() == Some("require")
                && let Some(args) = node.child_by_field_name("arguments")
            {
                // `require("x")` or `require "x"`
                let string = if args.kind() == "string" {
                    Some(args)
                } else {
                    named(args).into_iter().find(|a| a.kind() == "string")
                };
                if let Some(s) = string {
                    push(unquote(text(s, source)), Vec::new());
                }
                return;
            }
        }
        (Lang::Elixir, "call") => {
            let target = field(node, "target").unwrap_or_default();
            if matches!(target.as_str(), "alias" | "import" | "require" | "use")
                && let Some(args) = named(node).into_iter().find(|c| c.kind() == "arguments")
                && let Some(first) = named(args).into_iter().next()
            {
                // `alias A.B` or `alias A.{B, C}`
                let module = text(first, source);
                match module.split_once(".{") {
                    Some((base, list)) => push(
                        base.to_owned(),
                        list.trim_end_matches('}')
                            .split(',')
                            .map(|n| n.trim().to_owned())
                            .filter(|n| !n.is_empty())
                            .collect(),
                    ),
                    None => push(module.to_owned(), Vec::new()),
                }
                return;
            }
        }
        _ => {}
    }
    for child in named(node) {
        collect(lang, child, source, out);
    }
}

/// Everything the file imports, in source order.
pub fn imports(parsed: &Parsed) -> Vec<Import> {
    let mut out = Vec::new();
    collect(parsed.lang, parsed.tree.root_node(), &parsed.source, &mut out);
    out
}

/// Every identifier the file mentions (for "who references this symbol?").
pub fn identifiers(parsed: &Parsed) -> BTreeSet<String> {
    let mut out = Vec::new();
    idents_below(parsed.tree.root_node(), &parsed.source, &mut out);
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::parse;

    fn mods(lang: Lang, src: &str) -> Vec<(String, Vec<String>)> {
        imports(&parse(lang, src).unwrap()).into_iter().map(|i| (i.module, i.names)).collect()
    }

    #[test]
    fn imports_per_language() {
        assert_eq!(
            mods(Lang::Rust, "use crate::merge::{a, b};\nuse std::fmt;\nmod util;\n"),
            vec![
                ("crate::merge".into(), vec!["a".into(), "b".into()]),
                ("std".into(), vec!["fmt".into()]),
                ("self::util".into(), vec![])
            ]
        );
        assert_eq!(
            mods(Lang::TypeScript, "import { a, b as c } from './util';\nconst x = require(\"lodash\");\n"),
            vec![("./util".into(), vec!["a".into(), "b".into(), "c".into()]), ("lodash".into(), vec![])]
        );
        assert_eq!(
            mods(Lang::Python, "import os.path\nfrom .models import User, Team as T\n"),
            vec![("os.path".into(), vec![]), (".models".into(), vec!["User".into(), "Team".into()])]
        );
        assert_eq!(
            mods(Lang::Go, "package p\nimport (\n  \"fmt\"\n  x \"a/b\"\n)\n"),
            vec![("fmt".into(), vec![]), ("a/b".into(), vec![])]
        );
        assert_eq!(
            mods(Lang::Java, "import java.util.List;\nimport static a.B.c;\n"),
            vec![("java.util".into(), vec!["List".into()]), ("a.B".into(), vec!["c".into()])]
        );
    }

    #[test]
    fn imports_in_more_languages() {
        let none = Vec::<String>::new;
        assert_eq!(
            mods(Lang::C, "#include <stdio.h>\n#include \"util/str.h\"\n"),
            vec![("stdio.h".into(), none()), ("util/str.h".into(), none())]
        );
        assert_eq!(
            mods(
                Lang::CSharp,
                "using System;\nglobal using Acme.Models;\nusing static Acme.Util;\nusing J = Newtonsoft.Json;\n"
            ),
            vec![
                ("System".into(), none()),
                ("Acme.Models".into(), none()),
                ("Acme.Util".into(), none()),
                ("Newtonsoft.Json".into(), none())
            ]
        );
        assert_eq!(
            mods(Lang::Ruby, "require 'json'\nrequire_relative 'models/user'\nobj.require('x')\n"),
            vec![("json".into(), none()), ("./models/user".into(), none())]
        );
        assert_eq!(
            mods(
                Lang::Php,
                "<?php\nuse App\\Models\\User;\nuse App\\Support\\{Str, Arr};\nrequire_once 'lib/helpers.php';\n"
            ),
            vec![
                ("App\\Models".into(), vec!["User".into()]),
                ("App\\Support".into(), vec!["Str".into()]),
                ("App\\Support".into(), vec!["Arr".into()]),
                ("lib/helpers.php".into(), none())
            ]
        );
        assert_eq!(
            mods(Lang::Kotlin, "import com.acme.models.User\nimport kotlinx.coroutines.*\nimport a.b.c as d\n"),
            vec![
                ("com.acme.models".into(), vec!["User".into()]),
                ("kotlinx.coroutines".into(), none()),
                ("a.b".into(), vec!["c".into()])
            ]
        );
        assert_eq!(
            mods(Lang::Swift, "import Foundation\nimport UIKit\n"),
            vec![("Foundation".into(), none()), ("UIKit".into(), none())]
        );
        assert_eq!(
            mods(Lang::Scala, "import scala.collection.mutable\nimport com.acme.{User, Team}\nimport a.b._\n"),
            vec![
                ("scala.collection".into(), vec!["mutable".into()]),
                ("com.acme".into(), vec!["User".into(), "Team".into()]),
                ("a.b".into(), none())
            ]
        );
        assert_eq!(
            mods(Lang::Bash, "source ./lib/common.sh\n. env.sh\necho source\n"),
            vec![("./lib/common.sh".into(), none()), ("env.sh".into(), none())]
        );
        assert_eq!(
            mods(Lang::Lua, "local json = require(\"json\")\nlocal util = require \"lib.util\"\n"),
            vec![("json".into(), none()), ("lib.util".into(), none())]
        );
        assert_eq!(
            mods(
                Lang::Elixir,
                "defmodule A do\n  alias Acme.Repo\n  alias Acme.{User, Team}\n  import Ecto.Query\nend\n"
            ),
            vec![
                ("Acme.Repo".into(), none()),
                ("Acme".into(), vec!["User".into(), "Team".into()]),
                ("Ecto.Query".into(), none())
            ]
        );
    }

    #[test]
    fn identifiers_include_calls() {
        let ids = identifiers(&parse(Lang::Rust, "fn f() { helper(1); }").unwrap());
        assert!(ids.contains("helper") && ids.contains("f"));
    }
}
