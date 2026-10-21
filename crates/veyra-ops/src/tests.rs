//! End-to-end tests of the operation log against real temporary repositories.

use std::path::Path;

use veyra_core::Repo;
use veyra_core::checkout::SwitchTarget;
use veyra_core::commit::CommitRequest;
use veyra_core::worktree::Pathspec;

use crate::{ApplyConflict, OpLog, targets};

struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repo,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        let repo = Repo::init(&path, Some("main")).unwrap();
        let config = repo.config_path(veyra_core::config::Scope::Local).unwrap();
        veyra_core::config::set_in_file(&config, "user.name", "Test User").unwrap();
        veyra_core::config::set_in_file(&config, "user.email", "test@example.com").unwrap();
        veyra_core::config::set_in_file(&config, "core.autocrlf", "false").unwrap();
        let mut repo = repo;
        repo.reload().unwrap();
        Fixture { _dir: dir, repo }
    }

    fn root(&self) -> &Path {
        self.repo.workdir().unwrap()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.root().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root().join(path)).ok()
    }

    fn log(&self) -> OpLog<'_> {
        OpLog::open(&self.repo).unwrap()
    }

    /// `veyra add -A && veyra commit -m <msg>`, journaled like the CLI does.
    fn commit_all(&self, msg: &str) -> veyra_core::ObjectId {
        let log = self.log();
        let tx = log.begin("add", "veyra add -A", false).unwrap();
        self.repo.add(&Pathspec::all(), false).unwrap();
        tx.commit("add").unwrap();
        let tx = log.begin("commit", "veyra commit", false).unwrap();
        let id = self.repo.commit(&CommitRequest { message: format!("{msg}\n"), ..Default::default() }).unwrap();
        tx.commit(msg).unwrap();
        id
    }

    fn branch_tip(&self, name: &str) -> Option<veyra_core::ObjectId> {
        self.repo.ref_target(&format!("refs/heads/{name}")).unwrap()
    }
}

#[test]
fn undo_commit_moves_the_branch_back_and_keeps_changes_staged() {
    let fx = Fixture::new();
    fx.write("a.txt", "one\n");
    let first = fx.commit_all("first");
    fx.write("a.txt", "two\n");
    let second = fx.commit_all("second");
    assert_eq!(fx.branch_tip("main"), Some(second));

    let (target, _, _) = fx.log().undo(None, false, false, "veyra undo").unwrap();
    assert_eq!(target.kind, "commit");
    assert_eq!(fx.branch_tip("main"), Some(first));
    // Like `git reset --soft HEAD~1`: the change is still staged and in the worktree.
    let staged = fx.repo.status().unwrap().staged;
    assert_eq!(staged.len(), 1);
    assert_eq!(fx.read("a.txt").as_deref(), Some("two\n"));
}

#[test]
fn repeated_undo_walks_back_and_redo_walks_forward() {
    let fx = Fixture::new();
    fx.write("a.txt", "1\n");
    let c1 = fx.commit_all("c1");
    fx.write("a.txt", "2\n");
    let c2 = fx.commit_all("c2");

    let log = fx.log();
    log.undo(None, false, false, "veyra undo").unwrap(); // undo commit c2
    assert_eq!(fx.branch_tip("main"), Some(c1));
    log.undo(None, false, false, "veyra undo").unwrap(); // undo `add` of c2's change
    assert!(fx.repo.status().unwrap().staged.is_empty());
    assert_eq!(fx.read("a.txt").as_deref(), Some("2\n"), "undoing `add` never touches the worktree");

    log.redo(false, "veyra redo").unwrap(); // redo add
    log.redo(false, "veyra redo").unwrap(); // redo commit
    assert_eq!(fx.branch_tip("main"), Some(c2));
    assert!(log.redo(false, "veyra redo").is_err(), "nothing left to redo");
}

#[test]
fn a_new_operation_clears_the_redo_stack() {
    let fx = Fixture::new();
    fx.write("a.txt", "1\n");
    fx.commit_all("c1");
    fx.write("a.txt", "2\n");
    fx.commit_all("c2");
    let log = fx.log();
    log.undo(None, false, false, "veyra undo").unwrap();
    assert!(targets(&log.operations().unwrap()).1.is_some());
    let tx = log.begin("branch", "veyra branch x", false).unwrap();
    fx.repo.set_ref("refs/heads/x", fx.branch_tip("main").unwrap(), Some(None), "branch").unwrap();
    tx.commit("create branch x").unwrap();
    assert!(targets(&log.operations().unwrap()).1.is_none());
}

#[test]
fn undo_switch_restores_branch_and_files() {
    let fx = Fixture::new();
    fx.write("a.txt", "main\n");
    fx.commit_all("on main");
    let log = fx.log();

    let tx = log.begin("branch", "veyra switch -c feature", true).unwrap();
    let tip = fx.branch_tip("main").unwrap();
    fx.repo.set_ref("refs/heads/feature", tip, Some(None), "branch").unwrap();
    fx.repo.switch(SwitchTarget::Branch("refs/heads/feature".into()), false).unwrap();
    tx.commit("switch to feature").unwrap();

    fx.write("a.txt", "feature\n");
    fx.write("new.txt", "only on feature\n");
    fx.commit_all("on feature");

    let tx = log.begin("switch", "veyra switch main", true).unwrap();
    fx.repo.switch(SwitchTarget::Branch("refs/heads/main".into()), false).unwrap();
    tx.commit("switch to main").unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("main\n"));
    assert_eq!(fx.read("new.txt"), None);

    log.undo(None, false, false, "veyra undo").unwrap();
    assert_eq!(fx.repo.head_label().unwrap(), "feature");
    assert_eq!(fx.read("a.txt").as_deref(), Some("feature\n"));
    assert_eq!(fx.read("new.txt").as_deref(), Some("only on feature\n"));
    assert!(fx.repo.status().unwrap().worktree.is_clean());
}

#[test]
fn undo_refuses_to_clobber_later_edits_but_force_is_itself_undoable() {
    let fx = Fixture::new();
    fx.write("a.txt", "v1\n");
    fx.commit_all("c1");
    let log = fx.log();

    // An operation that rewrites a.txt: `restore --source` to an empty state, simulated with reset --hard.
    fx.write("a.txt", "v2\n");
    fx.commit_all("c2");
    let c1 = fx.repo.rev_parse_commit("HEAD~1").unwrap();
    let tx = log.begin("reset", "veyra reset --hard HEAD~1", true).unwrap();
    fx.repo.reset(c1, veyra_core::checkout::ResetMode::Hard).unwrap();
    tx.commit("reset --hard").unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("v1\n"));

    // The user edits the file again; undoing the reset must not silently overwrite that.
    fx.write("a.txt", "precious edit\n");
    let err = log.undo(None, false, false, "veyra undo").unwrap_err();
    assert!(err.downcast_ref::<ApplyConflict>().is_some(), "{err:#}");
    assert_eq!(fx.read("a.txt").as_deref(), Some("precious edit\n"));

    // --force overwrites… and the precious edit is still recoverable by undoing the undo's effect.
    log.undo(None, true, false, "veyra undo --force").unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("v2\n"));
    let ops = log.operations().unwrap();
    let forced = ops.last().unwrap();
    let (_, _, _) = log.restore(&forced.id, true, "veyra op restore --before").unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("precious edit\n"));
}

#[test]
fn plain_git_changes_are_recorded_as_external_and_can_be_undone() {
    let fx = Fixture::new();
    fx.write("a.txt", "1\n");
    let c1 = fx.commit_all("c1");
    // A commit made "outside Veyra": no transaction.
    fx.write("a.txt", "2\n");
    fx.repo.add(&Pathspec::all(), false).unwrap();
    let c2 = fx.repo.commit(&CommitRequest { message: "outside\n".into(), ..Default::default() }).unwrap();
    assert_eq!(fx.branch_tip("main"), Some(c2));

    let (target, _, _) = fx.log().undo(None, false, false, "veyra undo").unwrap();
    assert_eq!(target.kind, "external");
    assert_eq!(fx.branch_tip("main"), Some(c1));
    assert_eq!(fx.read("a.txt").as_deref(), Some("2\n"), "external undo leaves the user's files alone");
}

#[test]
fn checkpoints_recover_deleted_untracked_files() {
    let fx = Fixture::new();
    fx.write("tracked.txt", "t\n");
    fx.commit_all("c1");
    fx.write("notes/draft.md", "four hours of work\n");
    let log = fx.log();
    let cp = log.checkpoint(Some("before refactor"), "veyra checkpoint", false).unwrap().unwrap();
    assert!(
        log.checkpoint(None, "veyra checkpoint", false).unwrap().is_none(),
        "unchanged worktree → no new checkpoint"
    );

    std::fs::remove_file(fx.root().join("notes/draft.md")).unwrap();
    let (_, _, changed) = log.restore_checkpoint(&cp.id, &[], false, "veyra checkpoint restore").unwrap();
    assert_eq!(changed, vec!["notes/draft.md".to_owned()]);
    assert_eq!(fx.read("notes/draft.md").as_deref(), Some("four hours of work\n"));
}

#[test]
fn snapshots_are_protected_from_gc_by_the_keep_ref() {
    let fx = Fixture::new();
    fx.write("a.txt", "1\n");
    fx.commit_all("c1");
    fx.write("scratch.txt", "unsaved\n");
    let log = fx.log();
    let cp = log.checkpoint(None, "veyra checkpoint", false).unwrap().unwrap();
    let keep = fx.repo.ref_target(crate::KEEP_REF).unwrap().expect("keep ref exists");
    let keep_tree = fx.repo.commit_info(keep).unwrap().tree;
    let flat = fx.repo.flat_from_tree(keep_tree).unwrap();
    // The checkpoint's snapshot tree is reachable: its file appears under ops/<id>/before-worktree/.
    let needle = format!("ops/{}/{}/before-worktree/scratch.txt", &cp.id[..2], cp.id);
    assert!(flat.keys().any(|k| *k == needle.as_str()), "keep tree lacks {needle}");
}

#[test]
fn prune_drops_old_operations_but_keeps_the_newest() {
    let fx = Fixture::new();
    fx.write("a.txt", "1\n");
    fx.commit_all("c1");
    fx.write("a.txt", "2\n");
    fx.commit_all("c2");
    let log = fx.log();
    let before = log.operations().unwrap().len();
    assert_eq!(before, 4);
    let removed = log.prune(-1, 2).unwrap(); // everything is "old"; keep the newest 2
    assert_eq!(removed, 2);
    assert_eq!(log.operations().unwrap().len(), 2);
    assert!(fx.repo.ref_target(crate::KEEP_REF).unwrap().is_some());
}

#[test]
fn undo_of_add_only_unstages() {
    let fx = Fixture::new();
    fx.write("a.txt", "1\n");
    fx.commit_all("c1");
    fx.write("b.txt", "new\n");
    let log = fx.log();
    let tx = log.begin("add", "veyra add b.txt", false).unwrap();
    fx.repo.add(&Pathspec::all(), false).unwrap();
    tx.commit("add b.txt").unwrap();
    assert_eq!(fx.repo.status().unwrap().staged.len(), 1);
    log.undo(None, false, false, "veyra undo").unwrap();
    assert!(fx.repo.status().unwrap().staged.is_empty());
    assert_eq!(fx.read("b.txt").as_deref(), Some("new\n"));
}

#[test]
fn a_torn_final_line_from_a_crash_does_not_poison_later_operations() {
    let f = Fixture::new();
    f.write("a.txt", "one");
    f.commit_all("first");
    let path = f.repo.veyra_dir().join("oplog.jsonl");
    // Simulate a crash in the middle of appending a record.
    let mut data = std::fs::read(&path).unwrap();
    data.extend_from_slice(b"{\"id\":\"deadbeef\",\"time\":1,\"comm");
    std::fs::write(&path, data).unwrap();
    assert_eq!(f.log().operations().unwrap().len(), 2, "the torn line is ignored when reading");

    f.write("a.txt", "two");
    f.commit_all("second");
    let ops = f.log().operations().unwrap();
    assert_eq!(ops.len(), 4, "appending after a torn line keeps the log readable");
    assert!(!std::fs::read_to_string(&path).unwrap().contains("deadbeef"));
    f.log().undo(None, false, false, "veyra undo").unwrap();
    assert_eq!(f.read("a.txt").as_deref(), Some("two"));
}

mod properties {
    use proptest::prelude::*;

    /// One genuine journal record, produced once by a real operation.
    fn real_record() -> &'static str {
        static LINE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        LINE.get_or_init(|| {
            let f = super::Fixture::new();
            f.write("a.txt", "x");
            f.commit_all("first");
            let text = std::fs::read_to_string(f.repo.veyra_dir().join("oplog.jsonl")).unwrap();
            text.lines().last().unwrap().to_owned()
        })
    }

    proptest! {
        #[test]
        fn the_journal_parser_never_panics(text in "\\PC{0,400}") {
            let _ = crate::parse_log(&text);
        }

        #[test]
        fn a_torn_tail_is_always_tolerated(cut in 1usize..400) {
            let line = real_record();
            let full = format!("{line}\n");
            prop_assert_eq!(crate::parse_log(&full).unwrap().len(), 1);
            let torn = format!("{full}{}", &line[..cut.min(line.len() - 1)]);
            prop_assert_eq!(crate::parse_log(&torn).unwrap().len(), 1);
        }
    }
}
