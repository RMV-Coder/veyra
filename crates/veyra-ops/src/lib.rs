//! `veyra-ops` — the operation log behind `veyra undo`, `redo`, `op restore` and checkpoints.
//!
//! Every mutating Veyra command runs inside a [`Transaction`]: the repository's state is
//! captured before (including a worktree snapshot — the automatic checkpoint) and after,
//! and the pair is appended to `.git/veyra/oplog.jsonl`. All referenced objects are kept
//! reachable through `refs/veyra/keep` so `git gc` never prunes them. See docs/DESIGN.md §4.

mod apply;
mod log;
mod view;

pub use apply::{ApplyConflict, ApplyReport, targets};
pub use log::{KEEP_REF, OpLog, Operation, Settings, Transaction, new_op_id};
pub use view::{Captured, HeadView, View, capture, parse_id};

#[cfg(test)]
mod tests;
