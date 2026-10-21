//! `veyra-core` — the repository facade Veyra is built on.
//!
//! Everything here reads and writes a standard Git repository through
//! [gitoxide](https://github.com/GitoxideLabs/gitoxide). Nothing in this crate stores
//! history in a non-Git format.
//!
//! The central abstraction is [`FlatTree`]: a sorted map from repository-relative path
//! to `(mode, object id)`. HEAD, the index, worktree snapshots and arbitrary trees can
//! all be flattened into one, which lets status, diff, switch, reset and undo share the
//! same comparison and application logic.

pub mod annotate;
pub mod attest;
pub mod checkout;
pub mod commit;
pub mod config;
pub mod delta;
pub mod diff;
pub mod flat;
pub mod hooks;
pub mod index;
pub mod maintenance;
pub mod merge;
pub mod objects;
pub mod patch;
pub mod pick;
pub mod push;
pub mod refs;
pub mod remote;
pub mod repo;
pub mod sign;
pub mod sparse;
pub mod stash;
pub mod submodules;
pub mod time;
pub mod worktree;
pub mod worktrees;

pub use flat::{FlatEntry, FlatTree, PathChange};
pub use repo::{HeadState, Repo};

/// Re-export of the gitoxide crate so downstream crates use the exact same version.
pub use gix;
pub use gix::ObjectId;
pub use gix::bstr::{BStr, BString, ByteSlice};

/// Short, human-friendly form of an object id (git's default abbreviation length).
pub fn short(id: &ObjectId) -> String {
    id.to_hex_with_len(7).to_string()
}
