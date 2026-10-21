//! Syntax-aware analysis for Veyra, built on Tree-sitter: symbol extraction, semantic diff,
//! conservative structural merge, and dependency extraction for impact analysis.
//!
//! Everything here is pure (text in, results out); `veyra-core` decides where it's used.

pub mod deps;
pub mod diff;
pub mod items;
pub mod merge;

pub use deps::{Import, identifiers, imports};
pub use diff::{ChangeKind, FileSemanticDiff, SymbolChange, diff_parsed, semantic_diff};
pub use items::{Item, ItemKey, ItemKind, Lang, Parsed, items, parse};
pub use merge::{StructuralMerge, structural_merge};
