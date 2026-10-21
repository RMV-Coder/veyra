//! End-to-end tests of the `veyra` binary, using real Git as the compatibility oracle:
//! whatever Veyra writes must pass `git fsck` and agree with `git status` / `git log`.

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Env {
    /// A fresh repository with an isolated HOME and no system/global git config.
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".gitconfig"), "[user]\n\tname = Test User\n\temail = test@example.com\n[core]\n\tautocrlf = false\n[init]\n\tdefaultBranch = main\n").unwrap();
        let env = Env { _tmp: tmp, home, repo };
        env.veyra(&["init", env.repo.to_str().unwrap()]).assert().success();
        env
    }

    fn apply(&self, cmd: &mut Command) {
        cmd.env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("GIT_CONFIG_GLOBAL", self.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env_remove("VEYRA_AGENT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE");
    }

    fn veyra(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("veyra").unwrap();
        self.apply(&mut cmd);
        let dir = if self.repo.exists() { self.repo.clone() } else { self.home.clone() };
        cmd.current_dir(dir).args(args);
        cmd
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.veyra(args).output().unwrap();
        assert!(
            out.status.success(),
            "veyra {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        self.apply(&mut cmd);
        let out = cmd.current_dir(&self.repo).args(args).output().expect("git is installed");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.repo.join(path)).ok()
    }

    fn assert_git_clean_and_valid(&self) {
        self.git(&["fsck", "--strict", "--no-dangling"]);
        assert_eq!(self.git(&["status", "--porcelain"]), "", "git should see a clean worktree");
    }

    fn commit_file(&self, path: &str, content: &str, msg: &str) {
        self.write(path, content);
        self.ok(&["add", path]);
        self.ok(&["commit", "-m", msg]);
    }
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

macro_rules! require_git {
    () => {
        if !git_available() {
            eprintln!("skipping: git not installed");
            return;
        }
    };
}

#[test]
fn init_add_commit_produces_a_repository_git_accepts() {
    require_git!();
    let env = Env::new();
    env.write("README.md", "# Hello\n");
    env.write("src/main.rs", "fn main() {}\n");
    env.ok(&["add", "."]);
    let out = env.ok(&["commit", "-m", "feat: initial commit"]);
    assert!(out.contains("(root-commit)"), "{out}");
    assert!(out.contains("2 files changed"), "{out}");
    env.assert_git_clean_and_valid();
    assert_eq!(env.git(&["log", "--format=%s"]).trim(), "feat: initial commit");
    assert_eq!(env.git(&["ls-files"]), "README.md\nsrc/main.rs\n");
    assert_eq!(env.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "main");
}

#[test]
fn status_agrees_with_git() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    env.commit_file("b.txt", "b\n", "c2");
    env.write("a.txt", "changed\n");
    env.write("c.txt", "new\n");
    std::fs::remove_file(env.repo.join("b.txt")).unwrap();
    env.write("d.txt", "staged\n");
    env.ok(&["add", "d.txt"]);

    let git_short = env.git(&["status", "--porcelain"]);
    assert_eq!(git_short, " M a.txt\n D b.txt\nA  d.txt\n?? c.txt\n");
    let short = env.ok(&["status", "--short"]);
    assert_eq!(short, git_short, "veyra status --short matches git status --porcelain");
    let json: serde_json::Value = serde_json::from_str(&env.ok(&["status", "--json"])).unwrap();
    assert_eq!(json["branch"], "main");
    assert_eq!(json["untracked"], serde_json::json!(["c.txt"]));
    assert_eq!(json["clean"], false);
}

#[test]
fn undo_commit_then_redo() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "1\n", "first");
    env.commit_file("a.txt", "2\n", "second");
    let out = env.ok(&["undo", "commit"]);
    assert!(out.contains("Undid commit"), "{out}");
    assert_eq!(env.git(&["log", "--format=%s"]).trim(), "first");
    // The change is still staged, like `git reset --soft HEAD~1`.
    assert_eq!(env.git(&["status", "--porcelain"]), "M  a.txt\n");
    env.ok(&["redo"]);
    assert_eq!(env.git(&["log", "--format=%s"]), "second\nfirst\n");
    env.assert_git_clean_and_valid();
}

#[test]
fn undo_commit_refuses_when_the_last_operation_is_something_else() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "1\n", "first");
    env.ok(&["branch", "feature"]);
    env.veyra(&["undo", "commit"]).assert().failure().stderr(predicate::str::contains("not a commit"));
}

#[test]
fn reset_hard_is_recoverable_with_undo() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "committed\n", "c1");
    env.write("a.txt", "four hours of uncommitted work\n");
    env.write("notes.txt", "untracked notes\n");
    env.ok(&["reset", "--hard"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("committed\n"));
    assert_eq!(
        env.read("notes.txt").as_deref(),
        Some("untracked notes\n"),
        "reset --hard keeps untracked files, like git"
    );

    env.ok(&["undo"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("four hours of uncommitted work\n"));
    env.git(&["fsck", "--strict"]);
}

#[test]
fn discard_is_recoverable() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "v1\n", "c1");
    env.write("a.txt", "precious edit\n");
    env.ok(&["discard", "a.txt"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("v1\n"));
    env.ok(&["undo"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("precious edit\n"));
}

#[test]
fn switch_and_undo_switch() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "main\n", "on main");
    env.ok(&["switch", "-c", "feature"]);
    env.commit_file("feature.txt", "feature work\n", "on feature");
    env.ok(&["switch", "main"]);
    assert_eq!(env.read("feature.txt"), None);
    assert_eq!(env.git(&["branch", "--show-current"]).trim(), "main");
    env.assert_git_clean_and_valid();

    env.ok(&["undo"]);
    assert_eq!(env.git(&["branch", "--show-current"]).trim(), "feature");
    assert_eq!(env.read("feature.txt").as_deref(), Some("feature work\n"));
    env.assert_git_clean_and_valid();
}

#[test]
fn switch_refuses_to_overwrite_local_changes() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "main\n", "c1");
    env.ok(&["switch", "-c", "other"]);
    env.commit_file("a.txt", "other\n", "c2");
    env.ok(&["switch", "main"]);
    env.write("a.txt", "local edit\n");
    env.veyra(&["switch", "other"]).assert().failure().stderr(predicate::str::contains("would be overwritten"));
    assert_eq!(env.read("a.txt").as_deref(), Some("local edit\n"));
}

#[test]
fn changes_made_with_plain_git_can_be_undone() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "1\n", "veyra commit");
    env.write("a.txt", "2\n");
    env.git(&["commit", "-am", "git commit"]);
    let out = env.ok(&["undo"]);
    assert!(out.contains("external"), "{out}");
    assert_eq!(env.git(&["log", "--format=%s"]).trim(), "veyra commit");
    assert_eq!(env.read("a.txt").as_deref(), Some("2\n"));
}

#[test]
fn checkpoint_restores_an_accidentally_deleted_untracked_file() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    env.write("draft.md", "unsaved idea\n");
    let out = env.ok(&["checkpoint", "-m", "before cleanup"]);
    assert!(out.contains("Checkpoint"), "{out}");
    let list: serde_json::Value = serde_json::from_str(&env.ok(&["checkpoint", "list", "--json"])).unwrap();
    let id = list[0]["id"].as_str().unwrap().to_owned();

    std::fs::remove_file(env.repo.join("draft.md")).unwrap();
    env.ok(&["checkpoint", "restore", &id[..8]]);
    assert_eq!(env.read("draft.md").as_deref(), Some("unsaved idea\n"));
}

#[test]
fn checkpoints_survive_aggressive_gc() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    env.write("only-in-checkpoint.txt", "keep me\n");
    env.ok(&["checkpoint", "-m", "cp"]);
    let list: serde_json::Value = serde_json::from_str(&env.ok(&["checkpoint", "list", "--json"])).unwrap();
    let tree = list[0]["tree"].as_str().unwrap().to_owned();
    env.git(&["gc", "--prune=now", "--quiet"]);
    env.git(&["cat-file", "-e", &tree]);
    std::fs::remove_file(env.repo.join("only-in-checkpoint.txt")).unwrap();
    let id = list[0]["id"].as_str().unwrap().to_owned();
    env.ok(&["checkpoint", "restore", &id]);
    assert_eq!(env.read("only-in-checkpoint.txt").as_deref(), Some("keep me\n"));
}

#[test]
fn secret_scan_blocks_commits_until_allowed() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    let token = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8");
    env.write("config.ts", &format!("export const token = '{token}';\n"));
    env.ok(&["add", "config.ts"]);
    env.veyra(&["commit", "-m", "add config"]).assert().failure().stderr(predicate::str::contains("commit blocked"));
    assert_eq!(env.git(&["log", "--format=%s"]).trim(), "c1");
    env.ok(&["commit", "-m", "add config", "--allow-secrets"]);

    let scan = env.veyra(&["security", "scan", "--json"]).output().unwrap();
    assert!(!scan.status.success(), "high-confidence findings make the scan fail");
    let findings: serde_json::Value = serde_json::from_slice(&scan.stdout).unwrap();
    assert_eq!(findings[0]["rule"], "github-token");
    assert!(!String::from_utf8_lossy(&scan.stdout).contains(&token), "secrets are never printed");
    let fp = findings[0]["fingerprint"].as_str().unwrap().to_owned();
    env.ok(&["security", "allow", &fp]);
    env.ok(&["security", "scan"]);
}

#[test]
fn intents_record_why_and_guard_against_regressions() {
    require_git!();
    let env = Env::new();
    env.commit_file("src/payment/service.ts", "if (!req.idempotencyKey) throw new Error('missing key');\n", "c1");
    let created: serde_json::Value = serde_json::from_str(&env.ok(&[
        "intent",
        "create",
        "--json",
        "--title",
        "Prevent duplicate payment processing",
        "--kind",
        "bugfix",
        "--related",
        "PAY-142",
        "--guard-path",
        "src/payment/**",
        "--guard-contains",
        "idempotencyKey",
    ]))
    .unwrap();
    let id = created["id"].as_str().unwrap().to_owned();
    env.ok(&["add", ".veyra"]);
    env.ok(&["commit", "-m", "docs: record payment intent", "--intent", &id]);
    assert!(env.git(&["log", "-1", "--format=%B"]).contains(&format!("Intent: {id}")));
    env.ok(&["intent", "verify"]);

    // Removing the mechanism trips the guard.
    env.write("src/payment/service.ts", "createPayment(req);\n");
    env.veyra(&["intent", "verify"]).assert().failure().stdout(predicate::str::contains("POTENTIAL INTENT VIOLATION"));
    env.ok(&["add", "-A"]);
    env.veyra(&["commit", "-m", "refactor payments"])
        .assert()
        .success()
        .stderr(predicate::str::contains("contradict recorded intent"));
    let log = env.ok(&["intent", "log", &id]);
    assert!(log.contains("record payment intent"), "{log}");
}

#[test]
fn fast_forward_and_conflicted_merge_with_abort() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "feature"]);
    env.commit_file("a.txt", "feature\n", "feature change");
    env.ok(&["switch", "main"]);
    let out = env.ok(&["merge", "feature"]);
    assert!(out.contains("Fast-forward"), "{out}");
    env.assert_git_clean_and_valid();

    // Diverge and conflict.
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("a.txt", "topic\n", "topic change");
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "main\n", "main change");
    env.veyra(&["merge", "topic"]).assert().failure().stdout(predicate::str::contains("CONFLICT"));
    assert!(env.read("a.txt").unwrap().contains("<<<<<<<"));
    assert!(env.git(&["status", "--porcelain"]).contains("UU a.txt"), "git understands the conflict");
    env.ok(&["merge", "--abort"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("main\n"));
    env.assert_git_clean_and_valid();
}

#[test]
fn clean_three_way_merge_creates_a_merge_commit() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "base");
    env.ok(&["switch", "-c", "feature"]);
    env.commit_file("b.txt", "b\n", "add b");
    env.ok(&["switch", "main"]);
    env.commit_file("c.txt", "c\n", "add c");
    env.ok(&["merge", "feature"]);
    assert_eq!(env.git(&["log", "-1", "--format=%p"]).split_whitespace().count(), 2);
    assert_eq!(env.read("b.txt").as_deref(), Some("b\n"));
    env.assert_git_clean_and_valid();
}

#[test]
fn branches_tags_and_log() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "first");
    env.ok(&["branch", "create", "topic"]);
    env.ok(&["tag", "v1.0.0"]);
    env.ok(&["tag", "-a", "v1.0.1", "-m", "release"]);
    assert_eq!(env.git(&["tag"]), "v1.0.0\nv1.0.1\n");
    assert_eq!(env.git(&["cat-file", "-t", "v1.0.1"]).trim(), "tag");
    let oneline = env.ok(&["log", "--oneline"]);
    assert!(
        oneline.contains("HEAD -> main") && oneline.contains("tag: v1.0.0") && oneline.contains("topic"),
        "{oneline}"
    );
    env.ok(&["branch", "-m", "topic", "renamed"]);
    assert!(env.git(&["branch"]).contains("renamed"));
    env.ok(&["branch", "-d", "renamed"]);
    assert!(!env.git(&["branch"]).contains("renamed"));
    env.ok(&["undo"]);
    assert!(env.git(&["branch"]).contains("renamed"), "undo restores deleted branches");
    env.git(&["fsck", "--strict"]);
}

#[test]
fn diff_matches_git() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "one\ntwo\nthree\n", "c1");
    env.write("a.txt", "one\n2\nthree\n");
    let ours = env.ok(&["diff"]);
    let theirs = env.git(&["diff"]);
    let hunk = |s: &str| s.lines().skip_while(|l| !l.starts_with("@@")).collect::<Vec<_>>().join("\n");
    assert_eq!(hunk(&ours), hunk(&theirs));
    env.ok(&["add", "a.txt"]);
    assert_eq!(hunk(&env.ok(&["diff", "--staged"])), hunk(&env.git(&["diff", "--staged"])));
}

#[test]
fn crlf_handling_matches_git() {
    require_git!();
    let env = Env::new();
    env.ok(&["config", "core.autocrlf", "true"]);
    env.write("win.txt", "line one\r\nline two\r\n");
    env.ok(&["add", "win.txt"]);
    env.ok(&["commit", "-m", "crlf"]);
    // Stored with LF, like git does with autocrlf=true; and git sees nothing modified.
    assert_eq!(env.git(&["cat-file", "-p", "HEAD:win.txt"]), "line one\nline two\n");
    env.assert_git_clean_and_valid();
    env.write("win.txt", "line one\r\nline 2\r\n");
    env.ok(&["reset", "--hard"]);
    assert_eq!(env.read("win.txt").as_deref(), Some("line one\r\nline two\r\n"), "checkout smudges back to CRLF");
}

#[test]
fn clone_a_local_repository() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    let target = env.home.join("cloned");
    env.ok(&["clone", env.repo.to_str().unwrap(), target.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(target.join("a.txt")).unwrap(), "a\n");
    let mut cmd = Command::new("git");
    env.apply(&mut cmd);
    let out = cmd.current_dir(&target).args(["log", "--format=%s"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "c1");
}

#[test]
fn explain_and_doctor() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    env.veyra(&["explain", "reset", "--hard"]).assert().success().stdout(predicate::str::contains("Destructive"));
    let doc: serde_json::Value = serde_json::from_str(&env.ok(&["doctor", "--json"])).unwrap();
    assert_eq!(doc["fail"], 0);
    assert!(doc["checks"].as_array().unwrap().iter().any(|c| c["name"] == "Integrity" && c["status"] == "pass"));
}

#[test]
fn agent_mode_requires_confirmation_for_destructive_commands() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    env.veyra(&["reset", "--hard"])
        .env("VEYRA_AGENT", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--yes"));
    env.veyra(&["reset", "--hard", "--yes"]).env("VEYRA_AGENT", "1").assert().success();
}

#[test]
fn op_log_lists_operations_newest_first() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    let ops: serde_json::Value = serde_json::from_str(&env.ok(&["op", "log", "--json"])).unwrap();
    let kinds: Vec<&str> = ops.as_array().unwrap().iter().map(|o| o["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["commit", "add"]);
}

/// A bare repository next to `env.repo`, registered as `origin`.
fn bare_origin(env: &Env) -> PathBuf {
    let bare = env.home.join("origin.git");
    let mut cmd = Command::new("git");
    env.apply(&mut cmd);
    assert!(cmd.args(["init", "--bare", "-b", "main"]).arg(&bare).output().unwrap().status.success());
    env.ok(&["remote", "add", "origin", bare.to_str().unwrap()]);
    bare
}

fn git_in(env: &Env, dir: &std::path::Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    env.apply(&mut cmd);
    let out = cmd.current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn push_creates_and_fast_forwards_branches_on_a_bare_remote() {
    require_git!();
    let env = Env::new();
    let bare = bare_origin(&env);
    env.commit_file("a.txt", "one\n", "c1");
    env.commit_file("src/b.rs", "fn b() {}\n", "c2");

    let out = env.ok(&["push", "-u"]);
    assert!(out.contains("[new branch]"), "{out}");
    assert!(out.contains("set up to track 'origin/main'"), "{out}");
    assert_eq!(git_in(&env, &bare, &["log", "--format=%s", "main"]), "c2\nc1\n");
    git_in(&env, &bare, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(env.git(&["rev-parse", "origin/main"]), env.git(&["rev-parse", "main"]), "remote-tracking ref updated");
    assert_eq!(env.git(&["config", "branch.main.remote"]).trim(), "origin");

    env.commit_file("a.txt", "two\n", "c3");
    let out = env.ok(&["push"]);
    assert!(out.contains(".."), "fast-forward shows old..new: {out}");
    assert_eq!(git_in(&env, &bare, &["log", "-1", "--format=%s", "main"]).trim(), "c3");
    git_in(&env, &bare, &["fsck", "--strict", "--no-dangling"]);
    assert!(env.ok(&["push"]).contains("[up to date]"));
}

#[test]
fn push_rejects_non_fast_forward_unless_leased_or_forced() {
    require_git!();
    let env = Env::new();
    let bare = bare_origin(&env);
    env.commit_file("a.txt", "one\n", "c1");
    env.ok(&["push", "-u"]);
    // Someone else pushes c2 to the remote.
    let other = env.home.join("other");
    git_in(&env, &env.home, &["clone", "-q", bare.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("a.txt"), "theirs\n").unwrap();
    git_in(&env, &other, &["commit", "-qam", "their change"]);
    git_in(&env, &other, &["push", "-q", "origin", "main"]);
    // We rewrite our history locally.
    env.ok(&["commit", "--amend", "-m", "c1 amended"]);

    env.veyra(&["push"]).assert().failure().stdout(predicate::str::contains("[rejected]"));
    // Lease is stale: we never fetched their commit.
    env.veyra(&["push", "--force-with-lease"]).assert().failure().stdout(predicate::str::contains("stale info"));
    assert_eq!(git_in(&env, &bare, &["log", "-1", "--format=%s", "main"]).trim(), "their change");

    env.ok(&["fetch"]);
    let out = env.ok(&["push", "--force-with-lease"]);
    assert!(out.contains("forced update"), "{out}");
    assert_eq!(git_in(&env, &bare, &["log", "--format=%s", "main"]), "c1 amended\n");
}

#[test]
fn push_tags_and_delete_branches() {
    require_git!();
    let env = Env::new();
    let bare = bare_origin(&env);
    env.commit_file("a.txt", "one\n", "c1");
    env.ok(&["tag", "-a", "v1.0.0", "-m", "first release"]);
    env.ok(&["branch", "topic"]);
    env.ok(&["push", "origin", "main", "topic", "--tags"]);
    assert_eq!(git_in(&env, &bare, &["tag"]), "v1.0.0\n");
    assert_eq!(git_in(&env, &bare, &["cat-file", "-t", "v1.0.0"]).trim(), "tag", "annotated tag object was sent");
    assert!(git_in(&env, &bare, &["branch"]).contains("topic"));

    env.ok(&["push", "origin", "--delete", "topic"]);
    assert!(!git_in(&env, &bare, &["branch"]).contains("topic"));
    git_in(&env, &bare, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn undo_refuses_to_silently_unpublish_pushed_commits() {
    require_git!();
    let env = Env::new();
    bare_origin(&env);
    env.commit_file("a.txt", "one\n", "c1");
    env.commit_file("a.txt", "two\n", "c2");
    env.ok(&["push", "-u"]);
    env.veyra(&["undo"]).assert().failure().stderr(predicate::str::contains("already pushed"));
    assert_eq!(env.git(&["log", "-1", "--format=%s"]).trim(), "c2");
    env.ok(&["undo", "--force"]);
    assert_eq!(env.git(&["log", "-1", "--format=%s"]).trim(), "c1");
}

#[test]
fn agent_mode_requires_yes_for_force_push() {
    require_git!();
    let env = Env::new();
    bare_origin(&env);
    env.commit_file("a.txt", "one\n", "c1");
    env.veyra(&["push", "--force"])
        .env("VEYRA_AGENT", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--yes"));
}

#[test]
fn cherry_pick_keeps_the_author_and_revert_reverses() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "feature"]);
    env.write("feature.txt", "feature\n");
    env.ok(&["add", "feature.txt"]);
    env.veyra(&["commit", "-m", "feature work"])
        .env("GIT_AUTHOR_NAME", "Original Author")
        .env("GIT_AUTHOR_EMAIL", "orig@example.com")
        .assert()
        .success();
    env.ok(&["switch", "main"]);
    env.commit_file("main.txt", "main\n", "main work");

    env.ok(&["cherry-pick", "-x", "feature"]);
    assert_eq!(env.read("feature.txt").as_deref(), Some("feature\n"));
    let log = env.git(&["log", "-1", "--format=%an|%s|%b"]);
    assert!(log.starts_with("Original Author|feature work|"), "{log}");
    assert!(log.contains("cherry picked from commit"), "{log}");
    env.assert_git_clean_and_valid();

    env.ok(&["revert", "HEAD"]);
    assert_eq!(env.read("feature.txt"), None);
    assert!(env.git(&["log", "-1", "--format=%s"]).starts_with("Revert \"feature work\""));
    env.assert_git_clean_and_valid();
}

#[test]
fn conflicted_cherry_pick_continue_and_abort() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "feature"]);
    env.commit_file("a.txt", "feature\n", "feature edit");
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "main\n", "main edit");

    env.veyra(&["cherry-pick", "feature"]).assert().failure().stdout(predicate::str::contains("CONFLICT"));
    assert!(env.git(&["status"]).contains("cherry-pick"), "git sees the cherry-pick in progress");
    assert!(env.ok(&["status"]).contains("middle of a cherry-pick"));
    env.ok(&["cherry-pick", "--abort"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("main\n"));
    env.assert_git_clean_and_valid();

    env.veyra(&["cherry-pick", "feature"]).assert().failure();
    env.write("a.txt", "resolved\n");
    env.ok(&["add", "a.txt"]);
    env.ok(&["cherry-pick", "--continue"]);
    assert_eq!(env.git(&["log", "-1", "--format=%s"]).trim(), "feature edit");
    env.assert_git_clean_and_valid();
}

#[test]
fn rebase_replays_commits_and_undo_restores_the_branch() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("t1.txt", "1\n", "topic 1");
    env.commit_file("t2.txt", "2\n", "topic 2");
    let before = env.git(&["rev-parse", "topic"]);
    env.ok(&["switch", "main"]);
    env.commit_file("m.txt", "m\n", "main moved");
    env.ok(&["switch", "topic"]);

    let out = env.ok(&["rebase", "main"]);
    assert!(out.contains("Successfully rebased"), "{out}");
    assert_eq!(env.git(&["log", "--format=%s"]), "topic 2\ntopic 1\nmain moved\nbase\n");
    assert_eq!(env.git(&["branch", "--show-current"]).trim(), "topic");
    env.assert_git_clean_and_valid();
    assert!(env.ok(&["rebase", "main"]).contains("up to date"));

    env.ok(&["undo"]);
    assert_eq!(env.git(&["rev-parse", "topic"]), before, "undo restores the pre-rebase branch");
}

#[test]
fn rebase_stops_on_conflict_and_can_continue_or_abort() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("a.txt", "topic\n", "topic edit");
    env.commit_file("b.txt", "b\n", "topic adds b");
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "main\n", "main edit");
    env.ok(&["switch", "topic"]);
    let before = env.git(&["rev-parse", "topic"]);

    env.veyra(&["rebase", "main"]).assert().failure().stdout(predicate::str::contains("Could not apply"));
    assert!(env.ok(&["status"]).contains("rebasing"));
    env.ok(&["rebase", "--abort"]);
    assert_eq!(env.git(&["rev-parse", "topic"]), before);
    assert_eq!(env.git(&["branch", "--show-current"]).trim(), "topic");
    assert_eq!(env.read("a.txt").as_deref(), Some("topic\n"));
    env.assert_git_clean_and_valid();

    env.veyra(&["rebase", "main"]).assert().failure();
    env.write("a.txt", "merged\n");
    env.ok(&["add", "a.txt"]);
    env.ok(&["rebase", "--continue"]);
    assert_eq!(env.git(&["log", "--format=%s"]), "topic adds b\ntopic edit\nmain edit\nbase\n");
    assert_eq!(env.read("a.txt").as_deref(), Some("merged\n"));
    env.assert_git_clean_and_valid();
}

#[test]
fn stash_round_trips_and_interoperates_with_git_stash() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.write("a.txt", "work in progress\n");
    env.write("new.txt", "staged new file\n");
    env.ok(&["add", "new.txt"]);

    let out = env.ok(&["stash", "-m", "half-done feature"]);
    assert!(out.contains("Saved working directory"), "{out}");
    assert_eq!(env.read("a.txt").as_deref(), Some("base\n"));
    assert_eq!(env.read("new.txt"), None);
    env.assert_git_clean_and_valid();
    assert_eq!(env.git(&["stash", "list"]).trim(), "stash@{0}: On main: half-done feature", "git sees veyra's stash");

    env.ok(&["stash", "pop"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("work in progress\n"));
    assert_eq!(env.read("new.txt").as_deref(), Some("staged new file\n"));
    assert_eq!(
        env.git(&["status", "--porcelain"]),
        " M a.txt\nA  new.txt\n",
        "new files come back staged, edits unstaged"
    );
    assert_eq!(env.git(&["stash", "list"]), "");

    // A stash made by git, popped by veyra.
    env.git(&["stash", "push", "-m", "from git"]);
    assert!(env.ok(&["stash", "list"]).contains("from git"));
    env.ok(&["unshelve"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("work in progress\n"));
}

#[test]
fn stash_untracked_files_and_drop_keeps_git_in_sync() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.write("scratch.txt", "untracked\n");
    env.ok(&["stash", "push", "-u", "-m", "first"]);
    assert_eq!(env.read("scratch.txt"), None);
    env.write("a.txt", "second change\n");
    env.ok(&["shelve", "-m", "second"]);
    assert_eq!(env.git(&["stash", "list"]), "stash@{0}: On main: second\nstash@{1}: On main: first\n");

    env.ok(&["stash", "drop", "stash@{0}"]);
    assert_eq!(env.git(&["stash", "list"]), "stash@{0}: On main: first\n");
    env.ok(&["stash", "apply"]);
    assert_eq!(env.read("scratch.txt").as_deref(), Some("untracked\n"));
    assert_eq!(env.git(&["stash", "list"]).lines().count(), 1, "apply keeps the entry");
    std::fs::remove_file(env.repo.join("scratch.txt")).unwrap();
    env.ok(&["stash", "clear"]);
    assert_eq!(env.git(&["stash", "list"]), "");
    env.git(&["fsck", "--strict"]);
}

#[test]
fn stash_pop_with_conflicts_keeps_the_entry() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.write("a.txt", "stashed\n");
    env.ok(&["stash"]);
    env.commit_file("a.txt", "committed meanwhile\n", "c2");
    env.veyra(&["stash", "pop"]).assert().failure().stdout(predicate::str::contains("CONFLICT"));
    assert_eq!(env.git(&["stash", "list"]).lines().count(), 1);
    assert!(env.read("a.txt").unwrap().contains("<<<<<<<"));
}

fn install_hook(env: &Env, name: &str, body: &str) {
    let path = env.repo.join(".git").join("hooks").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn commit_hooks_run_like_git() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");

    install_hook(&env, "pre-commit", "echo 'lint failed' >&2\nexit 1");
    env.write("a.txt", "b\n");
    env.ok(&["add", "a.txt"]);
    env.veyra(&["commit", "-m", "blocked"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pre-commit hook failed"));
    assert_eq!(env.git(&["log", "--format=%s"]).trim(), "c1");
    env.ok(&["commit", "-m", "bypassed", "--no-verify"]);

    install_hook(&env, "pre-commit", "exit 0");
    install_hook(&env, "commit-msg", "printf '\nTicket: ABC-123\n' >> \"$1\"");
    install_hook(&env, "post-commit", "echo done > .git/post-commit-ran");
    env.commit_file("a.txt", "c\n", "feat: with hooks");
    let body = env.git(&["log", "-1", "--format=%B"]);
    assert!(body.contains("Ticket: ABC-123"), "commit-msg hook edited the message: {body}");
    assert!(env.repo.join(".git/post-commit-ran").exists());
}

#[test]
fn commit_all_restores_the_index_when_a_hook_fails() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    install_hook(&env, "pre-commit", "exit 1");
    env.write("a.txt", "changed\n");
    env.veyra(&["commit", "-am", "nope"]).assert().failure();
    assert_eq!(
        env.git(&["status", "--porcelain"]),
        " M a.txt\n",
        "the change is unstaged again, like git's temporary index"
    );
}

#[test]
fn pre_push_hook_sees_the_updates_and_can_block() {
    require_git!();
    let env = Env::new();
    let bare = bare_origin(&env);
    env.commit_file("a.txt", "a\n", "c1");
    install_hook(&env, "pre-push", "cat > .git/pre-push-input\necho \"$1 $2\" > .git/pre-push-args\nexit 1");
    env.veyra(&["push", "-u"]).assert().failure().stderr(predicate::str::contains("pre-push hook failed"));
    assert_eq!(git_in(&env, &bare, &["branch"]), "", "nothing was pushed");
    let input = std::fs::read_to_string(env.repo.join(".git/pre-push-input")).unwrap();
    let head = env.git(&["rev-parse", "HEAD"]);
    assert!(input.starts_with(&format!("main {} refs/heads/main 0000000", head.trim())), "{input}");
    assert!(std::fs::read_to_string(env.repo.join(".git/pre-push-args")).unwrap().starts_with("origin "));
    env.ok(&["push", "-u", "--no-verify"]);
    assert!(git_in(&env, &bare, &["branch"]).contains("main"));
}

#[test]
fn blame_matches_git() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "one\ntwo\nthree\n", "first");
    env.commit_file("a.txt", "one\nTWO\nthree\nfour\n", "second");
    let ours: serde_json::Value = serde_json::from_str(&env.ok(&["blame", "a.txt", "--json"])).unwrap();
    let git = env.git(&["blame", "-l", "-s", "a.txt"]);
    let git_ids: Vec<&str> =
        git.lines().map(|l| l.split_whitespace().next().unwrap().trim_start_matches('^')).collect();
    let our_ids: Vec<String> =
        ours.as_array().unwrap().iter().map(|l| l["commit"].as_str().unwrap().to_owned()).collect();
    assert_eq!(our_ids.len(), 4);
    for (o, g) in our_ids.iter().zip(&git_ids) {
        assert!(o.starts_with(g) || g.starts_with(o.as_str()), "{o} vs {g}");
    }
    assert_eq!(ours[1]["text"], "TWO");
    assert!(env.ok(&["blame", "a.txt", "-L", "2,2"]).lines().count() == 1);
}

#[test]
fn log_graph_draws_merges() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "base");
    env.ok(&["switch", "-c", "feature"]);
    env.commit_file("f.txt", "f\n", "feature work");
    env.ok(&["switch", "main"]);
    env.commit_file("m.txt", "m\n", "main work");
    env.ok(&["merge", "feature"]);
    let graph = env.ok(&["log", "--graph"]);
    assert!(graph.contains("|\\"), "{graph}");
    assert!(graph.contains("|/"), "{graph}");
    assert_eq!(graph.lines().filter(|l| l.contains('*')).count(), 4, "{graph}");
}

#[test]
fn clean_removes_untracked_files_recoverably() {
    require_git!();
    let env = Env::new();
    env.commit_file("src/a.txt", "a\n", "c1");
    env.write("src/junk.tmp", "junk\n");
    env.write("build/out.bin", "artifact\n");
    env.veyra(&["clean"]).assert().failure().stderr(predicate::str::contains("-f"));
    let dry = env.ok(&["clean", "-n"]);
    assert!(dry.contains("src/junk.tmp") && !dry.contains("build/out.bin"), "without -d, untracked dirs stay: {dry}");
    env.ok(&["clean", "-f", "-d"]);
    assert_eq!(env.read("src/junk.tmp"), None);
    assert_eq!(env.read("build/out.bin"), None);
    env.ok(&["undo"]);
    assert_eq!(env.read("build/out.bin").as_deref(), Some("artifact\n"));
}

#[test]
fn branch_rename_keeps_the_reflog_and_upstream() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    env.commit_file("a.txt", "b\n", "c2");
    env.ok(&["config", "branch.main.remote", "origin"]);
    let before = env.git(&["reflog", "show", "main"]).lines().count();
    env.ok(&["branch", "-m", "trunk"]);
    let after = env.git(&["reflog", "show", "trunk"]).lines().count();
    assert_eq!(after, before + 1, "history kept plus the rename entry");
    assert_eq!(env.git(&["config", "branch.trunk.remote"]).trim(), "origin");
    assert_eq!(env.git(&["branch", "--show-current"]).trim(), "trunk");
}

#[test]
fn interactive_rebase_drops_rewords_and_squashes() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("one.txt", "1\n", "c1");
    env.commit_file("two.txt", "2\n", "c2");
    env.commit_file("three.txt", "3\n", "c3");
    env.commit_file("four.txt", "4\n", "c4");

    // drop c2, fold c3 into c1 (fixup), reword c4.
    let seq = r#"sed -i -e 's/^pick \([0-9a-f]*\) c2$/drop \1 c2/' -e 's/^pick \([0-9a-f]*\) c3$/fixup \1 c3/' -e 's/^pick \([0-9a-f]*\) c4$/reword \1 c4/'"#;
    let out = env
        .veyra(&["rebase", "-i", "main"])
        .env("GIT_SEQUENCE_EDITOR", seq)
        .env("GIT_EDITOR", "sed -i -e '1s/.*/c4 reworded/'")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.git(&["log", "--format=%s"]), "c4 reworded\nc1\nbase\n");
    assert_eq!(env.read("two.txt"), None, "c2 was dropped");
    assert_eq!(env.read("three.txt").as_deref(), Some("3\n"), "c3 was folded into c1");
    assert_eq!(env.git(&["show", "--name-only", "--format=", "HEAD~1"]), "one.txt\nthree.txt\n");
    env.assert_git_clean_and_valid();
    env.ok(&["undo"]);
    assert_eq!(env.git(&["log", "--format=%s"]).lines().count(), 5, "undo restores all four commits");
}

#[test]
fn interactive_rebase_can_reorder_and_stop_to_edit() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.commit_file("x.txt", "x\n", "cx");
    env.commit_file("y.txt", "y\n", "cy");
    // Swap the two commits and stop at cx to amend it.
    let seq = "sed -i -n -e '/ cx$/{s/^pick/edit/;h;d}' -e '/ cy$/{p;x;p;d}' -e p";
    env.veyra(&["rebase", "-i", "HEAD~2"])
        .env("GIT_SEQUENCE_EDITOR", seq)
        .assert()
        .success()
        .stdout(predicate::str::contains("Stopped at"));
    env.write("x.txt", "x amended\n");
    env.ok(&["add", "x.txt"]);
    env.ok(&["commit", "--amend", "--no-edit"]);
    env.ok(&["rebase", "--continue"]);
    assert_eq!(env.git(&["log", "--format=%s"]), "cx\ncy\nbase\n");
    assert_eq!(env.read("x.txt").as_deref(), Some("x amended\n"));
    env.assert_git_clean_and_valid();
}

#[test]
fn add_patch_stages_selected_hunks_only() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n", "c1");
    env.write("a.txt", "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n");
    assert_cmd::Command::from_std(env.veyra(&["add", "-p"]))
        .write_stdin("y\nn\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Staged 1 change"));
    let staged = env.git(&["diff", "--cached"]);
    assert!(staged.contains("+A") && !staged.contains("+L"), "{staged}");
    let unstaged = env.git(&["diff"]);
    assert!(unstaged.contains("+L") && !unstaged.contains("+A"), "{unstaged}");
    env.git(&["fsck", "--strict"]);
}

#[test]
fn branch_cleanup_and_compare() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "base");
    env.ok(&["branch", "merged-one"]);
    env.ok(&["switch", "-c", "wip"]);
    env.commit_file("w.txt", "w\n", "wip work");
    env.ok(&["switch", "main"]);

    let cmp: serde_json::Value = serde_json::from_str(&env.ok(&["branch", "compare", "wip", "--json"])).unwrap();
    assert_eq!((cmp["ahead"].as_u64(), cmp["behind"].as_u64()), (Some(1), Some(0)));
    assert_eq!(cmp["changed_files"], serde_json::json!(["w.txt"]));

    assert!(env.ok(&["branch", "cleanup", "--dry-run"]).contains("merged-one"));
    env.ok(&["branch", "cleanup"]);
    let branches = env.git(&["branch"]);
    assert!(!branches.contains("merged-one") && branches.contains("wip"), "only merged branches go: {branches}");
    env.ok(&["undo"]);
    assert!(env.git(&["branch"]).contains("merged-one"));
}

#[test]
fn sync_fast_forwards_or_rebases_onto_the_upstream() {
    require_git!();
    let env = Env::new();
    let bare = bare_origin(&env);
    env.commit_file("a.txt", "a\n", "c1");
    env.ok(&["push", "-u"]);
    let other = env.home.join("other");
    git_in(&env, &env.home, &["clone", "-q", bare.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("b.txt"), "b\n").unwrap();
    git_in(&env, &other, &["add", "b.txt"]);
    git_in(&env, &other, &["commit", "-qm", "their commit"]);
    git_in(&env, &other, &["push", "-q", "origin", "main"]);

    env.ok(&["sync"]);
    assert_eq!(env.git(&["log", "--format=%s"]), "their commit\nc1\n", "fast-forwarded");

    // Now both sides have new commits: sync rebases ours on top, and --push publishes.
    std::fs::write(other.join("c.txt"), "c\n").unwrap();
    git_in(&env, &other, &["add", "c.txt"]);
    git_in(&env, &other, &["commit", "-qm", "their second"]);
    git_in(&env, &other, &["push", "-q", "origin", "main"]);
    env.commit_file("d.txt", "d\n", "our commit");
    env.ok(&["sync", "--push"]);
    assert_eq!(env.git(&["log", "--format=%s"]), "our commit\ntheir second\ntheir commit\nc1\n");
    assert_eq!(git_in(&env, &bare, &["log", "-1", "--format=%s", "main"]).trim(), "our commit");
}

#[test]
fn interactive_rebase_exec_and_break() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.commit_file("one.txt", "1\n", "c1");
    env.commit_file("two.txt", "2\n", "c2");

    let seq = r#"sed -i -e '/ c1$/a exec echo ok > .git/exec-ran' -e '/ c2$/a break'"#;
    env.veyra(&["rebase", "-i", "HEAD~2"])
        .env("GIT_SEQUENCE_EDITOR", seq)
        .assert()
        .success()
        .stdout(predicate::str::contains("break"));
    assert!(env.repo.join(".git/exec-ran").exists(), "exec ran after c1");
    assert!(env.ok(&["status"]).contains("rebasing"));
    env.ok(&["rebase", "--continue"]);
    assert_eq!(env.git(&["log", "--format=%s"]), "c2\nc1\nbase\n");

    let failing = r#"sed -i -e '/ c1$/a exec exit 3'"#;
    env.veyra(&["rebase", "-i", "HEAD~2"])
        .env("GIT_SEQUENCE_EDITOR", failing)
        .assert()
        .failure()
        .stdout(predicate::str::contains("exit code 3"));
    env.ok(&["rebase", "--continue"]);
    assert_eq!(env.git(&["log", "--format=%s"]), "c2\nc1\nbase\n");
    env.assert_git_clean_and_valid();
}

#[test]
fn incremental_pushes_send_deltas() {
    require_git!();
    let env = Env::new();
    let bare = bare_origin(&env);
    let big: String = (0..30_000).map(|i| format!("line number {i} of a large file\n")).collect();
    env.commit_file("big.txt", &big, "big file");
    env.ok(&["push", "-u", "--json"]);

    let edited = big.replacen("line number 15000 ", "LINE NUMBER 15000 (edited) ", 1);
    env.commit_file("big.txt", &edited, "small edit");
    let out: serde_json::Value = serde_json::from_str(&env.ok(&["push", "--json"])).unwrap();
    let bytes = out["bytes"].as_u64().unwrap();
    assert!(bytes < 4_000, "an incremental push of a one-line edit should be tiny, was {bytes} bytes");
    git_in(&env, &bare, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(git_in(&env, &bare, &["show", "main:big.txt"]), edited, "the remote reconstructed the delta exactly");
}

#[test]
fn branch_graph_and_explain() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "feature/auth"]);
    env.commit_file("auth/login.ts", "login\n", "auth");
    env.ok(&["switch", "-c", "feature/auth-refresh"]);
    env.commit_file("auth/refresh.ts", "refresh\n", "refresh");
    env.ok(&["switch", "main"]);
    env.ok(&["switch", "-c", "hotfix"]);
    env.commit_file("a.txt", "hotfix\n", "hotfix");
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "main moved\n", "main moved");

    let graph: serde_json::Value = serde_json::from_str(&env.ok(&["branch", "graph", "--json"])).unwrap();
    let parent_of = |b: &str| graph.as_array().unwrap().iter().find(|n| n["branch"] == b).unwrap()["parent"].clone();
    assert_eq!(parent_of("feature/auth-refresh"), "feature/auth");
    assert_eq!(parent_of("feature/auth"), "main");
    assert_eq!(parent_of("hotfix"), "main");
    let text = env.ok(&["branch", "graph"]);
    assert!(text.contains("└── feature/auth-refresh") || text.contains("├── feature/auth-refresh"), "{text}");

    let ex: serde_json::Value = serde_json::from_str(&env.ok(&["branch", "explain", "hotfix", "--json"])).unwrap();
    assert_eq!(ex["base"], "main");
    assert_eq!((ex["ahead"].as_u64(), ex["behind"].as_u64()), (Some(1), Some(1)));
    assert_eq!(ex["conflicts"], serde_json::json!(["a.txt"]), "in-memory merge preview finds the conflict");
    assert_eq!(env.git(&["status", "--porcelain"]), "", "the preview wrote nothing");
    let ex2: serde_json::Value =
        serde_json::from_str(&env.ok(&["branch", "explain", "feature/auth", "--json"])).unwrap();
    assert_eq!(ex2["areas"]["auth"], 1);
}

#[test]
fn add_patch_can_split_a_hunk() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "1\n2\n3\n4\n5\n", "c1");
    env.write("a.txt", "1\nTWO\n3\nFOUR\n5\n");
    assert_cmd::Command::from_std(env.veyra(&["add", "-p"])).write_stdin("s\nn\ny\n").assert().success();
    let staged = env.git(&["diff", "--cached"]);
    assert!(staged.contains("+FOUR") && !staged.contains("+TWO"), "{staged}");
}

/// Configure SSH signing with a fresh key; returns false if ssh-keygen is unavailable.
fn setup_ssh_signing(env: &Env) -> bool {
    let key = env.home.join("signing_key");
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "test@example.com", "-f"])
        .arg(&key)
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        return false;
    }
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    let allowed = env.home.join("allowed_signers");
    std::fs::write(&allowed, format!("test@example.com namespaces=\"git\" {}", public.trim())).unwrap();
    let path = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
    env.ok(&["config", "gpg.format", "ssh"]);
    env.ok(&["config", "user.signingkey", &path(&key)]);
    env.ok(&["config", "gpg.ssh.allowedSignersFile", &path(&allowed)]);
    true
}

#[test]
fn signed_commits_and_tags_verify_in_git_and_veyra() {
    require_git!();
    let env = Env::new();
    if !setup_ssh_signing(&env) {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }
    env.commit_file("a.txt", "a\n", "unsigned");
    env.write("a.txt", "b\n");
    env.ok(&["add", "a.txt"]);
    env.ok(&["commit", "-S", "-m", "signed"]);
    env.git(&["verify-commit", "HEAD"]);
    let v: serde_json::Value = serde_json::from_str(&env.ok(&["verify", "commit", "--json"])).unwrap();
    assert_eq!(v["valid"], true, "{v}");
    assert_eq!(v["format"], "ssh");

    env.ok(&["tag", "-s", "v1.0.0", "-m", "signed release"]);
    env.git(&["verify-tag", "v1.0.0"]);
    env.ok(&["verify", "tag", "v1.0.0"]);

    env.ok(&["config", "commit.gpgSign", "true"]);
    env.commit_file("a.txt", "c\n", "signed by config");
    env.git(&["verify-commit", "HEAD"]);
    let hist: serde_json::Value = serde_json::from_str(&env.ok(&["verify", "--history", "--json"])).unwrap();
    assert_eq!((hist["good"].as_u64(), hist["unsigned"].as_u64(), hist["bad"].as_u64()), (Some(2), Some(1), Some(0)));
    env.veyra(&["verify", "commit", "HEAD~2"]).assert().failure();
}

#[test]
fn maintenance_packs_and_prunes_without_losing_journal_objects() {
    require_git!();
    let env = Env::new();
    for i in 0..5 {
        env.commit_file("a.txt", &format!("version {i}\n"), &format!("c{i}"));
    }
    env.write("draft.txt", "only in a checkpoint\n");
    env.ok(&["checkpoint", "-m", "keep me"]);
    std::fs::remove_file(env.repo.join("draft.txt")).unwrap();
    // Garbage: an unreachable blob, written by git.
    let garbage = env.git(&["hash-object", "-w", "--stdin"]);
    let _ = garbage;

    let before: serde_json::Value = serde_json::from_str(&env.ok(&["storage", "stats", "--json"])).unwrap();
    assert!(before["loose_objects"].as_u64().unwrap() > 10);
    env.ok(&["maintenance", "run", "--prune-expire", "0s"]);
    let count = env.git(&["count-objects", "-v"]);
    assert!(count.contains("count: 0"), "everything was packed or pruned: {count}");
    env.git(&["fsck", "--strict", "--no-dangling"]);
    assert_eq!(env.git(&["log", "--format=%s"]).lines().count(), 5);

    // The checkpoint survived because the journal keeps its objects reachable.
    let list: serde_json::Value = serde_json::from_str(&env.ok(&["checkpoint", "list", "--json"])).unwrap();
    let id =
        list.as_array().unwrap().iter().find(|c| c["source"] == "keep me").unwrap()["id"].as_str().unwrap().to_owned();
    env.ok(&["checkpoint", "restore", &id]);
    assert_eq!(env.read("draft.txt").as_deref(), Some("only in a checkpoint\n"));
}

#[test]
fn worktrees_have_their_own_journal() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "base");
    let wt = env.home.join("feature-wt");
    env.ok(&["worktree", "add", wt.to_str().unwrap()]);
    let listing = env.git(&["worktree", "list"]);
    assert!(listing.contains("feature-wt") && listing.contains("[feature-wt]"), "{listing}");
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");

    // Work inside the linked worktree.
    let in_wt = |args: &[&str]| -> String {
        let mut cmd = Command::cargo_bin("veyra").unwrap();
        env.apply(&mut cmd);
        let out = cmd.current_dir(&wt).args(args).output().unwrap();
        assert!(out.status.success(), "veyra {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    std::fs::write(wt.join("b.txt"), "b\n").unwrap();
    in_wt(&["add", "b.txt"]);
    in_wt(&["commit", "-m", "work in worktree"]);
    assert_eq!(env.git(&["log", "-1", "--format=%s", "feature-wt"]).trim(), "work in worktree");
    let main_ops: serde_json::Value = serde_json::from_str(&env.ok(&["op", "log", "--json"])).unwrap();
    assert!(
        main_ops.as_array().unwrap().iter().all(|o| o["summary"] != "commit"),
        "the main journal doesn't see the worktree's commit"
    );
    in_wt(&["undo"]);
    assert_eq!(env.git(&["log", "-1", "--format=%s", "feature-wt"]).trim(), "base");
    env.git(&["fsck", "--strict"]);

    env.ok(&["worktree", "remove", "--force", wt.to_str().unwrap()]);
    assert!(!wt.exists());
    assert!(!env.git(&["worktree", "list"]).contains("feature-wt"));
}

#[test]
fn submodules_add_update_and_status() {
    require_git!();
    let env = Env::new();
    // A library repository to embed.
    let lib = env.home.join("lib");
    git_in(&env, &env.home, &["init", "-q", "-b", "main", lib.to_str().unwrap()]);
    std::fs::write(lib.join("lib.rs"), "pub fn lib() {}\n").unwrap();
    git_in(&env, &lib, &["add", "lib.rs"]);
    git_in(&env, &lib, &["commit", "-qm", "lib v1"]);
    let lib_v1 = git_in(&env, &lib, &["rev-parse", "HEAD"]).trim().to_owned();

    env.commit_file("app.rs", "fn main() {}\n", "app");
    env.ok(&["submodule", "add", lib.to_str().unwrap(), "vendor/lib"]);
    env.ok(&["commit", "-m", "add lib submodule"]);
    let status = env.git(&["submodule", "status"]);
    assert!(status.contains(&lib_v1) && status.contains("vendor/lib"), "{status}");
    assert_eq!(env.git(&["status", "--porcelain"]), "");

    // A fresh clone without submodules, completed by veyra.
    let clone = env.home.join("clone");
    git_in(
        &env,
        &env.home,
        &["-c", "protocol.file.allow=always", "clone", "-q", env.repo.to_str().unwrap(), clone.to_str().unwrap()],
    );
    let mut cmd = Command::cargo_bin("veyra").unwrap();
    env.apply(&mut cmd);
    let out = cmd.current_dir(&clone).args(["submodule", "update", "--init"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(clone.join("vendor/lib/lib.rs")).unwrap(), "pub fn lib() {}\n");
    let st = git_in(&env, &clone, &["submodule", "status"]);
    assert!(st.starts_with(' ') && st.contains(&lib_v1), "checked out at the recorded commit: {st}");
}

#[test]
fn sparse_checkout_matches_git_and_clone_scope_works() {
    require_git!();
    let env = Env::new();
    env.write("README.md", "root\n");
    env.write("frontend/app.ts", "app\n");
    env.write("backend/main.rs", "fn main() {}\n");
    env.write("packages/ui/button.ts", "button\n");
    env.write("packages/types/t.ts", "types\n");
    env.write(".veyra/scopes.toml", "[scope.web]\npaths = [\"frontend\", \"packages/ui\"]\n");
    env.ok(&["add", "-A"]);
    env.ok(&["commit", "-m", "monorepo"]);

    env.ok(&["sparse", "set", "frontend", "packages/ui"]);
    assert_eq!(env.read("backend/main.rs"), None);
    assert_eq!(env.read("packages/types/t.ts"), None);
    assert_eq!(env.read("frontend/app.ts").as_deref(), Some("app\n"));
    assert_eq!(env.git(&["status", "--porcelain"]), "", "git agrees nothing is modified or deleted");
    assert_eq!(env.git(&["sparse-checkout", "list"]), "frontend\npackages/ui\n");
    // Commits still contain the whole tree.
    env.commit_file("frontend/app.ts", "app v2\n", "edit in cone");
    assert!(env.git(&["ls-tree", "-r", "--name-only", "HEAD"]).contains("backend/main.rs"));

    env.ok(&["sparse", "disable"]);
    assert_eq!(env.read("backend/main.rs").as_deref(), Some("fn main() {}\n"));
    assert_eq!(env.git(&["status", "--porcelain"]), "");

    let clone = env.home.join("web-only");
    env.ok(&["clone", env.repo.to_str().unwrap(), clone.to_str().unwrap(), "--scope", "web"]);
    assert!(clone.join("frontend/app.ts").exists() && clone.join("packages/ui/button.ts").exists());
    assert!(!clone.join("backend").exists(), "backend is outside the scope");
    assert_eq!(git_in(&env, &clone, &["status", "--porcelain"]), "");
}

#[test]
fn sha256_repositories_work_end_to_end() {
    require_git!();
    let env = Env::new();
    let repo = env.home.join("sha256-repo");
    env.ok(&["init", "--object-format", "sha256", repo.to_str().unwrap()]);
    let run = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("veyra").unwrap();
        env.apply(&mut cmd);
        let out = cmd.current_dir(&repo).args(args).output().unwrap();
        assert!(out.status.success(), "veyra {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    run(&["add", "a.txt"]);
    run(&["commit", "-m", "first sha256 commit"]);
    std::fs::write(repo.join("a.txt"), "hello again\n").unwrap();
    run(&["commit", "-am", "second"]);
    run(&["undo"]);
    assert_eq!(git_in(&env, &repo, &["rev-parse", "--show-object-format"]).trim(), "sha256");
    assert_eq!(git_in(&env, &repo, &["rev-parse", "HEAD"]).trim().len(), 64);
    assert_eq!(git_in(&env, &repo, &["log", "--format=%s"]), "first sha256 commit\n");
    git_in(&env, &repo, &["fsck", "--strict"]);
}

#[test]
fn attestations_are_git_notes_and_verify() {
    require_git!();
    let env = Env::new();
    let bare = bare_origin(&env);
    env.commit_file("a.txt", "a\n", "c1");
    env.ok(&["attest", "run", "--", "exit 0"]);
    env.ok(&["attest", "review", "--reviewer", "Ada <ada@example.com>", "-m", "LGTM"]);
    env.veyra(&["attest", "run", "--", "exit 4"]).assert().failure();

    let list: serde_json::Value = serde_json::from_str(&env.ok(&["attest", "list", "--json"])).unwrap();
    let atts = list["attestations"].as_array().unwrap();
    assert_eq!(atts.len(), 3);
    assert_eq!(atts[0]["statement"]["_type"], "https://in-toto.io/Statement/v1");
    assert_eq!(atts[0]["statement"]["predicate"]["success"], true);
    assert_eq!(atts[2]["statement"]["predicate"]["exit_code"], 4);
    let note = env.git(&["notes", "--ref", "veyra/attestations", "show", "HEAD"]);
    assert_eq!(note.lines().count(), 3, "git reads the same notes");

    if setup_ssh_signing(&env) {
        env.ok(&["attest", "review", "--sign", "-m", "signed approval"]);
        let v: serde_json::Value = serde_json::from_str(&env.ok(&["verify", "--provenance", "--json"])).unwrap();
        assert_eq!(v["bad"], 0);
        assert!(v["attestations"].as_array().unwrap().iter().any(|a| a["status"] == "signature good"), "{v}");
    }

    env.ok(&["push", "-u"]);
    env.ok(&["push", "origin", "refs/notes/veyra/attestations"]);
    assert!(
        git_in(&env, &bare, &["notes", "--ref", "veyra/attestations", "list"])
            .contains(&env.git(&["rev-parse", "HEAD"]).trim().to_string())
    );
}

const SEM_BASE: &str = "fn a() -> u32 { 1 }\nfn b() -> u32 { 1 }\n";
const SEM_MERGED: &str = "fn a() -> u32 { 2 }\nfn b() -> u32 { 3 }\n";

/// Two branches change adjacent functions: Git's line merge conflicts, Veyra merges per symbol.
fn diverge_on_adjacent_functions(env: &Env) {
    env.commit_file("lib.rs", SEM_BASE, "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("lib.rs", "fn a() -> u32 { 2 }\nfn b() -> u32 { 1 }\n", "change a");
    env.ok(&["switch", "main"]);
    env.commit_file("lib.rs", "fn a() -> u32 { 1 }\nfn b() -> u32 { 3 }\n", "change b");
}

#[test]
fn structural_merge_resolves_conflicts_in_different_functions() {
    require_git!();
    let env = Env::new();
    diverge_on_adjacent_functions(&env);
    let out = env.ok(&["merge", "topic"]);
    assert!(out.contains("Auto-resolved lib.rs"), "{out}");
    assert!(out.contains("function a: taken from theirs"), "{out}");
    assert_eq!(env.read("lib.rs").unwrap(), SEM_MERGED);
    assert_eq!(env.git(&["log", "-1", "--format=%p"]).split_whitespace().count(), 2);
    assert!(env.git(&["log", "-1", "--format=%B"]).contains("Veyra-Structural-Merge: lib.rs"));
    env.assert_git_clean_and_valid();

    // `veyra undo` restores the pre-merge state like any other operation.
    env.ok(&["undo"]);
    assert_eq!(env.git(&["log", "-1", "--format=%s"]).trim(), "change b");
}

#[test]
fn structural_merge_can_be_disabled_and_leaves_same_symbol_conflicts() {
    require_git!();
    let env = Env::new();
    diverge_on_adjacent_functions(&env);
    env.ok(&["config", "veyra.structuralMerge", "false"]);
    env.veyra(&["merge", "topic"]).assert().failure().stdout(predicate::str::contains("CONFLICT"));
    env.ok(&["merge", "--abort"]);

    // `stage`: resolved and staged, but the merge waits for review.
    env.ok(&["config", "veyra.structuralMerge", "stage"]);
    let out = env.ok(&["merge", "topic"]);
    assert!(out.contains("staged for review"), "{out}");
    assert_eq!(env.read("lib.rs").unwrap(), SEM_MERGED);
    assert_eq!(env.git(&["status", "--porcelain"]), "M  lib.rs\n");
    env.ok(&["commit", "--no-edit"]);
    assert_eq!(env.git(&["log", "-1", "--format=%p"]).split_whitespace().count(), 2);
    env.ok(&["undo"]);

    // Both sides change the same function: always a conflict.
    env.ok(&["config", "veyra.structuralMerge", "true"]);
    env.ok(&["switch", "-c", "other"]);
    env.commit_file("lib.rs", "fn a() -> u32 { 7 }\nfn b() -> u32 { 3 }\n", "a=7");
    env.ok(&["switch", "main"]);
    env.commit_file("lib.rs", "fn a() -> u32 { 8 }\nfn b() -> u32 { 3 }\n", "a=8");
    env.veyra(&["merge", "other"]).assert().failure().stdout(predicate::str::contains("CONFLICT"));
    assert!(env.git(&["status", "--porcelain"]).contains("UU lib.rs"));
}

#[test]
fn cherry_pick_uses_structural_merge() {
    require_git!();
    let env = Env::new();
    diverge_on_adjacent_functions(&env);
    let out = env.ok(&["cherry-pick", "topic"]);
    assert!(out.contains("Auto-resolved lib.rs"), "{out}");
    assert_eq!(env.read("lib.rs").unwrap(), SEM_MERGED);
    env.assert_git_clean_and_valid();
}

#[test]
fn semantic_diff_lists_changed_symbols() {
    require_git!();
    let env = Env::new();
    env.commit_file(
        "lib.rs",
        "fn keep() {}\nfn old_name(x: u32) -> u32 { x * 2 }\nfn gone() {}\nfn tweak() -> u32 { 1 }\n",
        "base",
    );
    env.write(
        "lib.rs",
        "fn keep() {}\nfn new_name(x: u32) -> u32 { x * 2 }\nfn tweak(y: u8) -> u32 { 1 }\nfn fresh() {}\n",
    );
    let out = env.ok(&["diff", "--semantic"]);
    assert!(out.contains("function old_name → new_name"), "{out}");
    assert!(out.contains("- function gone"), "{out}");
    assert!(out.contains("+ function fresh"), "{out}");
    assert!(out.contains("function tweak  signature changed"), "{out}");
    assert!(!out.contains("keep"), "{out}");
    let json: serde_json::Value = serde_json::from_str(&env.ok(&["diff", "--functions", "--json"])).unwrap();
    assert_eq!(json[0]["language"], "Rust");
    assert_eq!(json[0]["changes"].as_array().unwrap().len(), 4);
}

#[test]
fn impact_reports_dependents_tests_and_risk_with_evidence() {
    require_git!();
    let env = Env::new();
    env.write("src/util.ts", "export function parseConfig(text: string) {\n  return JSON.parse(text);\n}\n");
    env.write("src/app.ts", "import { parseConfig } from './util';\nexport const cfg = parseConfig('{}');\n");
    env.write("src/other.ts", "export function unrelated() { return 1; }\n");
    env.write("test/util.test.ts", "import { parseConfig } from '../src/util';\ntest('x', () => parseConfig('{}'));\n");
    env.ok(&["add", "."]);
    env.ok(&["commit", "-m", "base"]);
    env.write(
        "src/util.ts",
        "export function parseConfig(text: string, strict: boolean) {\n  return JSON.parse(text);\n}\n",
    );

    let out = env.ok(&["impact"]);
    assert!(out.contains("function parseConfig  signature changed"), "{out}");
    assert!(out.contains("high   src/app.ts"), "{out}");
    assert!(out.contains("imports src/util.ts"), "{out}");
    assert!(!out.contains("src/other.ts"), "{out}");
    assert!(out.contains("Limitations"), "{out}");

    let json: serde_json::Value = serde_json::from_str(&env.ok(&["impact", "--json"])).unwrap();
    assert_ne!(json["risk"]["level"], "low", "{json}");
    let tests: serde_json::Value = serde_json::from_str(&env.ok(&["impact", "--tests", "--json"])).unwrap();
    assert_eq!(tests[0]["path"], "test/util.test.ts", "{tests}");

    // Committed changes can be analysed too.
    env.ok(&["commit", "-am", "strict mode"]);
    assert!(env.ok(&["impact", "--commit", "HEAD"]).contains("src/app.ts"));
}

#[test]
fn rerere_records_replays_and_shares_resolutions_with_git() {
    require_git!();
    let env = Env::new();
    env.ok(&["rerere", "enable"]);
    env.commit_file("a.txt", "start\nbase\nend\n", "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("a.txt", "start\ntopic\nend\n", "topic");
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "start\nmain\nend\n", "main");
    let resolved = "start\nmain and topic\nend\n";

    // First time: a normal conflict, resolved by hand, recorded on commit.
    env.veyra(&["merge", "topic"]).assert().failure();
    assert!(env.ok(&["merge", "resolutions"]).contains("a.txt"));
    env.write("a.txt", resolved);
    env.ok(&["add", "a.txt"]);
    let out = env.ok(&["commit", "--no-edit"]);
    assert!(out.contains("Recorded the resolution of a.txt"), "{out}");
    assert!(env.ok(&["rerere", "list"]).contains("resolved"));

    // Second time: replayed into the file; the conflict stays until the user reviews and adds it.
    env.ok(&["undo"]); // back to the stopped merge…
    env.ok(&["merge", "--abort"]); // …and to before it
    let out = env.veyra(&["merge", "topic"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("using a recorded resolution"), "{stdout}");
    assert_eq!(env.read("a.txt").as_deref(), Some(resolved));
    assert!(env.git(&["status", "--porcelain"]).contains("UU a.txt"));
    env.ok(&["merge", "--abort"]);

    // With auto-staging the merge only needs a commit.
    env.ok(&["rerere", "enable", "--auto-stage"]);
    let out = env.ok(&["merge", "topic"]);
    assert!(out.contains("(staged)"), "{out}");
    assert_eq!(env.git(&["status", "--porcelain"]), "M  a.txt\n");
    env.ok(&["merge", "--abort"]);

    // Git reads Veyra's recording (same rr-cache format and conflict ids).
    let mut git = Command::new("git");
    env.apply(&mut git);
    let out = git.current_dir(&env.repo).args(["merge", "topic"]).output().unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(all.contains("previous resolution"), "{all}");
    assert_eq!(env.read("a.txt").as_deref(), Some(resolved));
}

/// Configure a fake local model: a shell script that answers by task and logs every request.
fn fake_ai(env: &Env) -> PathBuf {
    let script = env.home.join("fake-ai.sh");
    let log = env.home.join("ai-requests.log");
    let body = r#"#!/bin/sh
input=$(cat)
printf '%s\n----\n' "$input" >> "LOG"
case "$input" in
  *change_units*) printf '%s
' '{"commits":[{"message":"feat: add a","files":["a.txt"]},{"message":"docs: add b","files":["b.txt"]}]}' ;;
  *conflicted_file*) printf '%s
' '{"resolved":"start\nmain and topic\nend\n","confidence":"CONF","explanation":"Kept both."}' ;;
  *staged_diff*) printf '%s
' '{"subject":"feat: add greeting","body":"Adds a friendly greeting."}' ;;
  *findings*|*senior\ engineer*) printf '%s
' '{"summary":"Looks fine.","findings":[{"severity":"medium","file":"a.txt","line":1,"title":"Typo","detail":"Hello is misspelled."}]}' ;;
  *) printf '%s
' 'This change adds a greeting file.' ;;
esac
"#
    .replace("LOG", &log.to_string_lossy().replace('\\', "/"));
    std::fs::write(&script, body).unwrap();
    let cmd = format!("sh {}", script.to_string_lossy().replace('\\', "/"));
    env.ok(&["ai", "configure", "--provider", "command", "--command", &cmd]);
    log
}

#[test]
fn ai_is_off_by_default_and_never_sends_without_configuration() {
    require_git!();
    let env = Env::new();
    assert!(env.ok(&["ai"]).contains("AI provider: off"));
    env.write("a.txt", "hello\n");
    env.ok(&["add", "a.txt"]);
    env.veyra(&["commit", "--smart", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("AI features are off"));
    assert_eq!(env.git(&["for-each-ref", "refs/heads"]), "", "no commit was made");
}

#[test]
fn smart_commit_proposes_a_message_and_needs_approval() {
    require_git!();
    let env = Env::new();
    let log = fake_ai(&env);
    env.write("a.txt", "hello\n");
    env.ok(&["add", "a.txt"]);

    // Without approval (no terminal, no --yes) nothing is committed.
    env.veyra(&["commit", "--smart"]).assert().failure().stderr(predicate::str::contains("needs approval"));
    assert_eq!(env.git(&["for-each-ref", "refs/heads"]), "", "no commit was made");

    let out = env.ok(&["commit", "--smart", "--yes"]);
    assert!(out.contains("feat: add greeting"), "{out}");
    let msg = env.git(&["log", "-1", "--format=%B"]);
    assert!(msg.starts_with("feat: add greeting\n\nAdds a friendly greeting."), "{msg}");
    assert!(msg.contains("Assisted-by: local model via veyra ai"), "{msg}");
    let sent = std::fs::read_to_string(&log).unwrap();
    assert!(sent.contains("+hello"), "the staged diff is what gets sent: {sent}");
    env.assert_git_clean_and_valid();
}

#[test]
fn secrets_are_never_sent_and_dry_run_sends_nothing() {
    require_git!();
    let env = Env::new();
    let log = fake_ai(&env);
    env.write("config.txt", &format!("token = ghp_{}\n", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8"));
    env.ok(&["add", "config.txt"]);
    env.veyra(&["ai", "explain", "--staged"]).assert().failure().stderr(predicate::str::contains("nothing was sent"));
    assert!(!log.exists(), "nothing reached the provider");

    env.write("config.txt", "token = (from the environment)\n");
    env.ok(&["add", "config.txt"]);
    let out = env.ok(&["commit", "--smart", "--dry-run"]);
    assert!(out.contains("Would send to: local command"), "{out}");
    assert!(out.contains("<staged_diff>"), "{out}");
    assert!(!log.exists(), "a dry run sends nothing");
}

#[test]
fn split_commit_validates_the_plan_and_makes_each_commit() {
    require_git!();
    let env = Env::new();
    fake_ai(&env);
    env.commit_file("README.md", "# x\n", "init");
    env.write("a.txt", "a\n");
    env.write("b.txt", "b\n");
    env.ok(&["add", "a.txt", "b.txt"]);
    let out = env.ok(&["commit", "--split", "--yes"]);
    assert!(out.contains("Proposed: 2 commits"), "{out}");
    assert_eq!(env.git(&["log", "--format=%s", "-2"]), "docs: add b\nfeat: add a\n");
    assert_eq!(env.git(&["show", "--name-only", "--format=", "HEAD~1"]).trim(), "a.txt");
    env.assert_git_clean_and_valid();

    // A plan that doesn't cover the staged files is refused, and nothing changes.
    env.write("c.txt", "c\n");
    env.write("d.txt", "d\n");
    env.ok(&["add", "c.txt", "d.txt"]);
    env.veyra(&["commit", "--split", "--yes"]).assert().failure().stderr(predicate::str::contains("not usable"));
    assert_eq!(env.git(&["status", "--porcelain"]), "A  c.txt\nA  d.txt\n");
}

#[test]
fn ai_explain_and_review_print_the_answer() {
    require_git!();
    let env = Env::new();
    fake_ai(&env);
    env.commit_file("a.txt", "helo\n", "init");
    env.write("a.txt", "hello\n");
    assert!(env.ok(&["ai", "explain"]).contains("adds a greeting file"));
    let out = env.ok(&["ai", "review"]);
    assert!(out.contains("Typo") && out.contains("a.txt:1"), "{out}");
    let json: serde_json::Value = serde_json::from_str(&env.ok(&["ai", "review", "--json"])).unwrap();
    assert_eq!(json["findings"][0]["severity"], "medium");
}

#[test]
fn assisted_merge_needs_approval_and_uncertain_proposals_a_human() {
    require_git!();
    let env = Env::new();
    let log = fake_ai(&env);
    env.commit_file("a.txt", "start\nbase\nend\n", "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("a.txt", "start\ntopic\nend\n", "topic");
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "start\nmain\nend\n", "main");

    // Low confidence: even --yes doesn't accept it.
    let script = env.home.join("fake-ai.sh");
    let s = std::fs::read_to_string(&script).unwrap();
    std::fs::write(&script, s.replace("CONF", "low")).unwrap();
    env.veyra(&["merge", "topic"]).assert().failure();
    let out = env.veyra(&["ai", "resolve", "--yes"]).output().unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(all.contains("needs interactive approval"), "{all}");
    assert!(env.read("a.txt").unwrap().contains("<<<<<<<"));
    env.ok(&["merge", "--abort"]);

    // High confidence with --yes: resolved, staged, and undoable.
    std::fs::write(&script, s.replace("CONF", "high")).unwrap();
    let out = env.veyra(&["merge", "topic", "--assist", "--yes"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Resolved and staged a.txt"), "{stdout}");
    assert_eq!(env.read("a.txt").as_deref(), Some("start\nmain and topic\nend\n"));
    assert_eq!(env.git(&["status", "--porcelain"]), "M  a.txt\n");
    assert!(std::fs::read_to_string(&log).unwrap().contains("<ours_version>"));
    env.ok(&["undo"]);
    assert!(env.read("a.txt").unwrap().contains("<<<<<<<"), "undo brings the conflict back");
}

#[test]
fn analyze_flags_risky_migrations_and_infrastructure() {
    require_git!();
    let env = Env::new();
    env.commit_file("infra/main.tf", "resource \"aws_db_instance\" \"main\" {}\n", "infra");
    env.write(
        "db/migrations/002_cleanup.sql",
        "ALTER TABLE users DROP COLUMN email;\nCREATE INDEX idx ON orders (created_at);\n",
    );
    env.write("infra/main.tf", "resource \"aws_security_group\" \"web\" {\n  cidr_blocks = [\"0.0.0.0/0\"]\n}\n");
    env.write("src/app.py", "print('no risk here')\n");
    env.ok(&["add", "."]);

    let out = env.ok(&["analyze", "--staged"]);
    assert!(out.contains("db/migrations/002_cleanup.sql:1") && out.contains("[drop-column]"), "{out}");
    assert!(out.contains("[index-without-concurrently]"), "{out}");
    assert!(out.contains("[tf-resource-removed]") && out.contains("aws_db_instance.main"), "{out}");
    assert!(out.contains("[tf-open-ingress]"), "{out}");
    assert!(!out.contains("app.py"), "{out}");

    let only: serde_json::Value =
        serde_json::from_str(&env.ok(&["analyze", "--staged", "--migration", "--json"])).unwrap();
    assert!(only.as_array().unwrap().iter().all(|f| f["area"] == "migration"), "{only}");

    // CI gate, repository rules and disabling built-ins.
    env.veyra(&["analyze", "--staged", "--fail-on", "high"]).assert().failure();
    env.write(
        ".veyra/analyzers.toml",
        "disable = [\"index-without-concurrently\"]\n\n[[rule]]\nid = \"no-print\"\npaths = [\"src/**\"]\npattern = \"print\\\\(\"\nseverity = \"low\"\nmessage = \"use the logger\"\n",
    );
    let out = env.ok(&["analyze", "--staged"]);
    assert!(!out.contains("index-without-concurrently"), "{out}");
    assert!(out.contains("src/app.py:1") && out.contains("[no-print]"), "{out}");
}

#[test]
fn intent_suggest_keeps_only_guards_that_hold() {
    require_git!();
    let env = Env::new();
    env.commit_file("src/pay.ts", "export function charge() { return idempotencyKey(); }\n", "pay");
    let script = env.home.join("intent-ai.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > /dev/null\nprintf '%s\\n' '{\"title\":\"Charges are idempotent\",\"kind\":\"decision\",\"objective\":\"Never double-charge\",\"constraints\":[\"Keep the idempotency key\"],\"guards\":[{\"paths\":[\"src/pay.ts\"],\"must_contain\":\"idempotencyKey\",\"must_not_contain\":null,\"message\":\"keep idempotency\"},{\"paths\":[\"src/pay.ts\"],\"must_contain\":\"retryTwice\",\"must_not_contain\":null,\"message\":\"bogus\"}]}'\n",
    )
    .unwrap();
    let cmd = format!("sh {}", script.to_string_lossy().replace('\\', "/"));
    env.ok(&["ai", "configure", "--command", &cmd]);
    let out = env.veyra(&["intent", "suggest", "src", "--yes"]).output().unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{all}");
    assert!(all.contains("Intent recorded") && all.contains("dropped a proposed guard"), "{all}");
    let list = env.ok(&["intent", "list"]);
    assert!(list.contains("Charges are idempotent"), "{list}");
    assert!(env.ok(&["intent", "verify"]).contains("All guards of 1 intent hold"));
}

#[test]
fn concurrent_veyra_commands_serialize_through_the_journal_lock() {
    require_git!();
    let env = std::sync::Arc::new(Env::new());
    env.commit_file("a.txt", "a\n", "base");
    let threads: Vec<_> = (0..6)
        .map(|i| {
            let env = env.clone();
            std::thread::spawn(move || {
                let branch = format!("b{i}");
                let tag = format!("t{i}");
                let outs = [
                    env.veyra(&["branch", "create", &branch]).output().unwrap(),
                    env.veyra(&["tag", &tag]).output().unwrap(),
                    env.veyra(&["checkpoint", "-m", &format!("cp {i}")]).output().unwrap(),
                ];
                let err: String = outs.iter().map(|o| String::from_utf8_lossy(&o.stderr).to_string()).collect();
                (outs[0].status.success(), outs[1].status.success(), outs[2].status.success(), err)
            })
        })
        .collect();
    for t in threads {
        let (a, b, c, err) = t.join().unwrap();
        assert!(a && b && c, "every command succeeds (they queue on the lock): {err}");
    }
    let branches = env.git(&["branch", "--format=%(refname:short)"]);
    let tags = env.git(&["tag"]);
    for i in 0..6 {
        assert!(branches.contains(&format!("b{i}")), "{branches}");
        assert!(tags.contains(&format!("t{i}")), "{tags}");
    }
    // The journal stayed well-formed: every operation is recorded exactly once.
    let log: serde_json::Value = serde_json::from_str(&env.ok(&["op", "log", "--json"])).unwrap();
    let kinds: Vec<&str> = log.as_array().unwrap().iter().filter_map(|o| o["kind"].as_str()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "branch").count(), 6, "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "tag").count(), 6, "{kinds:?}");
    env.git(&["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn veyra_and_git_running_at_the_same_time_stay_consistent() {
    require_git!();
    let env = std::sync::Arc::new(Env::new());
    env.commit_file("a.txt", "0\n", "base");
    let git_env = env.clone();
    let git = std::thread::spawn(move || {
        for i in 1..=8 {
            git_env.write("g.txt", &format!("{i}\n"));
            git_env.git(&["add", "g.txt"]);
            git_env.git(&["commit", "-q", "-m", &format!("git {i}")]);
        }
    });
    let mut failures = Vec::new();
    for _ in 0..8 {
        for args in [&["status"][..], &["log", "--oneline", "-n", "3"][..], &["checkpoint"][..]] {
            let out = env.veyra(args).output().unwrap();
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            // A command may lose a race with Git's own index.lock; it must say so, not corrupt anything.
            if !out.status.success() && !err.contains("lock") {
                failures.push(format!("veyra {args:?}: {err}"));
            }
        }
    }
    git.join().unwrap();
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!(env.git(&["rev-list", "--count", "HEAD"]).trim(), "9");
    env.git(&["fsck", "--strict", "--no-dangling"]);
    // Veyra notices what Git did and can still undo it.
    env.ok(&["status"]);
    // Undo reverts the most recent outside change (what Git did since Veyra last looked)…
    let undo = env.ok(&["undo"]);
    assert!(undo.contains("Undid"), "{undo}");
    let after_undo: u32 = env.git(&["rev-list", "--count", "HEAD"]).trim().parse().unwrap();
    assert!(after_undo < 9, "{after_undo}");
    env.git(&["fsck", "--strict", "--no-dangling"]);
    // …and redo brings it back.
    env.ok(&["redo"]);
    assert_eq!(env.git(&["rev-list", "--count", "HEAD"]).trim(), "9");
}

/// Three commits on main, an annotated tag on the first and a lightweight one on the second.
fn tagged_history(env: &Env) {
    env.commit_file("a.txt", "one\ntwo\nthree\n", "feat: first");
    env.git(&["tag", "-a", "v1.0.0", "-m", "release 1"]);
    env.commit_file("b.txt", "b\n", "fix(core): second");
    env.git(&["tag", "light"]);
    env.commit_file("a.txt", "one\nTWO\nthree\n", "docs: third");
}

#[test]
fn describe_shortlog_and_grep_agree_with_git() {
    require_git!();
    let env = Env::new();
    tagged_history(&env);
    assert_eq!(env.ok(&["describe"]), env.git(&["describe"]));
    assert_eq!(env.ok(&["describe", "--tags"]), env.git(&["describe", "--tags"]));
    assert_eq!(env.ok(&["describe", "HEAD~2"]), env.git(&["describe", "HEAD~2"]));
    assert_eq!(env.ok(&["describe", "--long", "HEAD~2"]), env.git(&["describe", "--long", "HEAD~2"]));
    assert_eq!(env.ok(&["shortlog", "-s"]).trim(), env.git(&["shortlog", "-s", "HEAD"]).trim());

    assert_eq!(env.ok(&["grep", "-n", "TWO"]), "a.txt:2:TWO\n");
    assert_eq!(env.ok(&["grep", "-i", "-l", "two"]), env.git(&["grep", "-i", "-l", "two"]));
    assert_eq!(env.ok(&["grep", "-c", "o"]), env.git(&["grep", "-c", "o"]));
    assert_eq!(env.ok(&["grep", "two", "v1.0.0"]).trim(), "v1.0.0:a.txt:two");
    env.veyra(&["grep", "nothing-matches-this"]).assert().failure();
}

#[test]
fn bisect_run_finds_the_first_bad_commit() {
    require_git!();
    let env = Env::new();
    for i in 0..10 {
        let content = if i >= 6 { format!("broken {i}\n") } else { format!("fine {i}\n") };
        env.commit_file("state.txt", &content, &format!("c{i}"));
    }
    env.ok(&["bisect", "start", "HEAD", "HEAD~9"]);
    let out = env.ok(&["bisect", "run", "grep", "-q", "fine", "state.txt"]);
    assert!(out.contains("is the first bad commit") && out.contains("c6"), "{out}");
    env.ok(&["bisect", "reset"]);
    assert_eq!(env.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "main");
    env.assert_git_clean_and_valid();
}

#[test]
fn format_patch_and_am_interoperate_with_git() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "one\ntwo\nthree\n", "base");
    env.commit_file("a.txt", "one\nTWO\nthree\n", "change two\n\nWith a body.");
    env.commit_file("new.txt", "hello", "add new file");
    env.ok(&["format-patch", "HEAD~2", "-o", "out"]);
    let files: Vec<_> = std::fs::read_dir(env.repo.join("out")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 2, "{files:?}");

    // Git applies Veyra's patches…
    let other = env.home.join("other");
    git_in(&env, &env.home, &["clone", "-q", env.repo.to_str().unwrap(), other.to_str().unwrap()]);
    git_in(&env, &other, &["reset", "-q", "--hard", "HEAD~2"]);
    let mut am = vec!["am", "-q"];
    let paths: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
    am.extend(paths.iter().map(String::as_str));
    git_in(&env, &other, &am);
    assert_eq!(git_in(&env, &other, &["log", "--format=%s|%b", "-1", "HEAD~1"]).trim(), "change two|With a body.");
    assert_eq!(std::fs::read_to_string(other.join("new.txt")).unwrap(), "hello");

    // …and Veyra applies Git's.
    let git_patches = git_in(&env, &other, &["format-patch", "--stdout", "HEAD~2"]);
    std::fs::write(env.home.join("git.mbox"), git_patches).unwrap();
    env.ok(&["reset", "--hard", "HEAD~2", "--yes"]);
    std::fs::remove_dir_all(env.repo.join("out")).unwrap();
    let out = env.ok(&["am", env.home.join("git.mbox").to_str().unwrap()]);
    assert!(out.contains("Applied") && out.contains("add new file"), "{out}");
    assert_eq!(env.git(&["log", "--format=%an|%s", "-2"]), "Test User|add new file\nTest User|change two\n");
    assert_eq!(env.read("new.txt").as_deref(), Some("hello"));
    env.assert_git_clean_and_valid();
}

#[test]
fn apply_takes_git_diffs_and_can_reverse_them() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "one\ntwo\nthree\n", "base");
    env.write("a.txt", "zero\none\nTWO\nthree\n");
    env.write("b.txt", "new\n");
    env.git(&["add", "-N", "b.txt"]);
    let diff = env.git(&["diff"]);
    std::fs::write(env.home.join("p.diff"), &diff).unwrap();
    env.git(&["checkout", "--", "a.txt"]);
    env.git(&["rm", "-q", "--cached", "b.txt"]);
    std::fs::remove_file(env.repo.join("b.txt")).unwrap();

    let p = env.home.join("p.diff");
    env.ok(&["apply", "--check", p.to_str().unwrap()]);
    env.ok(&["apply", "--index", p.to_str().unwrap()]);
    assert_eq!(env.read("a.txt").as_deref(), Some("zero\none\nTWO\nthree\n"));
    assert_eq!(env.git(&["status", "--porcelain"]), "M  a.txt\nA  b.txt\n");
    env.ok(&["apply", "--index", "-R", p.to_str().unwrap()]);
    assert_eq!(env.git(&["status", "--porcelain"]), "");
    // A patch whose context doesn't match is refused, with nothing written.
    env.commit_file("a.txt", "something else\n", "diverge");
    env.veyra(&["apply", p.to_str().unwrap()]).assert().failure();
    assert_eq!(env.read("a.txt").as_deref(), Some("something else\n"));
}

#[test]
fn archives_list_the_same_files_as_git_archive() {
    require_git!();
    let env = Env::new();
    env.commit_file("src/main.rs", "fn main() {}\n", "a");
    env.commit_file("README.md", "# x\n", "b");
    let tar = env.home.join("v.tar");
    env.ok(&["archive", "-o", tar.to_str().unwrap(), "--prefix", "proj"]);
    let mut cmd = Command::new("tar");
    let listing = cmd.arg("-tf").arg(env.home.join("v.tar")).output().expect("tar is installed");
    let mut ours: Vec<String> = String::from_utf8_lossy(&listing.stdout).lines().map(str::to_owned).collect();
    ours.sort();
    assert_eq!(ours, vec!["proj/README.md", "proj/src/main.rs"]);
    // Git reads the commit id from the pax header, like with `git archive`.
    let mut get = Command::new("git");
    env.apply(&mut get);
    let out =
        get.arg("get-tar-commit-id").stdin(std::fs::File::open(env.home.join("v.tar")).unwrap()).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), env.git(&["rev-parse", "HEAD"]).trim());

    env.ok(&["archive", "-o", env.home.join("v.tgz").to_str().unwrap()]);
    let listing = Command::new("tar").arg("-tzf").arg(env.home.join("v.tgz")).output().unwrap();
    assert!(listing.status.success(), "{}", String::from_utf8_lossy(&listing.stderr));
    assert!(String::from_utf8_lossy(&listing.stdout).contains("src/main.rs"));
    env.ok(&["archive", "--format", "zip", "-o", env.home.join("v.zip").to_str().unwrap()]);
    assert!(std::fs::read(env.home.join("v.zip")).unwrap().starts_with(b"PK\x03\x04"));
}

#[test]
fn bundles_round_trip_through_git() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "1\n", "one");
    env.ok(&["tag", "-a", "v1", "-m", "v1"]);
    env.commit_file("a.txt", "2\n", "two");
    let full = env.home.join("full.bundle");
    env.ok(&["bundle", "create", full.to_str().unwrap()]);
    git_in(&env, &env.repo, &["bundle", "verify", full.to_str().unwrap()]);
    let clone = env.home.join("from-bundle");
    git_in(&env, &env.home, &["clone", "-q", full.to_str().unwrap(), clone.to_str().unwrap()]);
    assert_eq!(git_in(&env, &clone, &["log", "--format=%s"]), "two\none\n");

    // Incremental bundle with a prerequisite, unbundled by Veyra into the clone's history.
    env.commit_file("a.txt", "3\n", "three");
    let inc = env.home.join("inc.bundle");
    let out = env.ok(&["bundle", "create", inc.to_str().unwrap(), "v1..main"]);
    assert!(out.contains("prerequisite"), "{out}");
    let heads = env.ok(&["bundle", "list-heads", inc.to_str().unwrap()]);
    assert!(heads.contains("refs/heads/main"), "{heads}");
    let mut cmd = env.veyra(&["bundle", "unbundle", inc.to_str().unwrap()]);
    let out = cmd.current_dir(&clone).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let head = env.git(&["rev-parse", "HEAD"]);
    assert_eq!(git_in(&env, &clone, &["cat-file", "-t", head.trim()]).trim(), "commit");
    git_in(&env, &clone, &["fsck", "--no-dangling"]);
}

#[test]
fn notes_ls_remote_and_plumbing_match_git() {
    require_git!();
    let env = Env::new();
    let bare = bare_origin(&env);
    tagged_history(&env);
    env.ok(&["push", "-u", "origin", "main", "--tags"]);

    env.ok(&["notes", "add", "-m", "reviewed by A"]);
    assert_eq!(env.git(&["notes", "show"]), "reviewed by A\n");
    env.ok(&["notes", "append", "-m", "and B"]);
    assert_eq!(env.git(&["notes", "show"]), "reviewed by A\n\nand B\n");
    env.ok(&["notes", "remove"]);
    assert!(env.veyra(&["notes", "show"]).output().unwrap().stdout.is_empty());

    let ours = env.ok(&["remote", "refs", "origin"]);
    let theirs = git_in(&env, &env.repo, &["ls-remote", bare.to_str().unwrap()]);
    let sorted = |s: &str| {
        let mut v: Vec<String> = s.lines().map(str::to_owned).collect();
        v.sort();
        v
    };
    assert_eq!(sorted(&ours), sorted(&theirs));

    assert_eq!(env.ok(&["write-tree"]), env.git(&["write-tree"]));
    assert_eq!(env.ok(&["merge-base", "HEAD", "HEAD~2"]), env.git(&["merge-base", "HEAD", "HEAD~2"]));
    env.ok(&["merge-base", "--is-ancestor", "HEAD~2", "HEAD"]);
    env.veyra(&["merge-base", "--is-ancestor", "HEAD", "HEAD~2"]).assert().failure();
    let fmt = "%(refname:short) %(objecttype) %(subject)";
    assert_eq!(
        env.ok(&["for-each-ref", "--format", fmt, "refs/tags/"]),
        env.git(&["for-each-ref", "--format", fmt, "refs/tags/"])
    );
    let tree = env.git(&["rev-parse", "HEAD^{tree}"]);
    let made = env.ok(&["commit-tree", tree.trim(), "-p", "HEAD", "-m", "plumbing"]);
    assert_eq!(env.git(&["cat-file", "-t", made.trim()]).trim(), "commit");
    env.ok(&["update-ref", "refs/heads/plumbed", made.trim()]);
    assert_eq!(env.git(&["log", "-1", "--format=%s", "plumbed"]).trim(), "plumbing");
    env.ok(&["undo"]);
    assert!(env.git(&["branch", "--list", "plumbed"]).is_empty(), "update-ref is undoable");
    env.assert_git_clean_and_valid();
}

#[test]
fn release_notes_prepare_and_verify() {
    require_git!();
    let env = Env::new();
    tagged_history(&env);
    env.commit_file("c.txt", "c\n", "feat!: new api");
    let notes = env.ok(&["release", "notes"]);
    assert!(notes.contains("### ⚠ Breaking changes") && notes.contains("new api"), "{notes}");
    assert!(notes.contains("### Bug fixes") && notes.contains("**core:** second"), "{notes}");
    assert!(!notes.contains("first"), "only changes since v1.0.0: {notes}");

    env.write("dirty.txt", "x");
    env.ok(&["add", "dirty.txt"]);
    let out = env.veyra(&["release", "prepare", "v2.0.0"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("FAIL  clean working tree"));
    env.ok(&["commit", "-m", "chore: dirty"]);
    let out = env.ok(&["release", "prepare", "v2.0.0"]);
    assert!(out.contains("Tagged v2.0.0"), "{out}");
    assert!(env.git(&["tag", "-n20", "v2.0.0"]).contains("new api"));
    let v = env.ok(&["release", "verify", "v2.0.0"]);
    assert!(v.contains("not signed") && v.contains("Commits in this release: 4"), "{v}");
}

/// One raw HTTP request; returns (status code, body).
fn http(port: u16, request: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default();
    (status, body)
}

struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serve_is_local_token_protected_read_only_and_serves_the_data() {
    require_git!();
    use std::io::BufRead;
    let env = Env::new();
    env.commit_file("a.txt", "one\n", "feat: first");
    env.commit_file("a.txt", "two\n", "fix: second\n\nAssisted-by: test-model");
    let port = 17000 + (std::process::id() % 2000) as u16;
    let mut cmd = env.veyra(&["serve", "--no-open", "--port", &port.to_string()]);
    let mut child = KillOnDrop(cmd.stdout(std::process::Stdio::piped()).spawn().unwrap());
    let mut line = String::new();
    std::io::BufReader::new(child.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let url = line.split_whitespace().last().unwrap().to_owned();
    let port: u16 = url.split(':').nth(2).unwrap().split('/').next().unwrap().parse().unwrap();
    let token = url.split("token=").nth(1).unwrap().trim().to_owned();
    let get = |path: &str, host: &str, tok: Option<&str>| {
        let auth = tok.map(|t| format!("X-Veyra-Token: {t}\r\n")).unwrap_or_default();
        http(port, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n"))
    };
    let host = format!("127.0.0.1:{port}");

    // The page itself loads without a token; data never does.
    assert_eq!(get("/", &host, None).0, 200);
    assert_eq!(get("/api/summary", &host, None).0, 403);
    assert_eq!(get("/api/summary", &host, Some("wrong")).0, 403);
    // DNS rebinding: a foreign Host is refused even with the token.
    assert_eq!(get("/api/summary", &format!("evil.example:{port}"), Some(&token)).0, 421);
    // Read-only.
    let (status, _) = http(
        port,
        &format!("POST /api/summary HTTP/1.1\r\nHost: {host}\r\nX-Veyra-Token: {token}\r\nContent-Length: 0\r\n\r\n"),
    );
    assert_eq!(status, 405);

    let (status, body) = get("/api/summary", &host, Some(&token));
    assert_eq!(status, 200, "{body}");
    let summary: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(summary["branch"], "main");
    assert_eq!(summary["undo"]["kind"], "commit");

    let log: serde_json::Value = serde_json::from_str(&get("/api/log", &host, Some(&token)).1).unwrap();
    assert_eq!(log["commits"][0]["summary"], "fix: second");
    assert_eq!(log["commits"][0]["assisted"][0], "test-model");
    let id = log["commits"][0]["id"].as_str().unwrap().to_owned();
    let commit: serde_json::Value =
        serde_json::from_str(&get(&format!("/api/commit/{id}"), &host, Some(&token)).1).unwrap();
    assert_eq!(commit["signature"]["status"], "unsigned");
    assert_eq!(commit["files"][0]["path"], "a.txt");
    let diff = get(&format!("/api/diff/{id}?path=a.txt"), &host, Some(&token)).1;
    assert!(diff.contains("-one") && diff.contains("+two"), "{diff}");
    let journal: serde_json::Value = serde_json::from_str(&get("/api/journal", &host, Some(&token)).1).unwrap();
    assert!(journal["operations"].as_array().unwrap().iter().any(|o| o["kind"] == "commit"));
    for endpoint in ["checkpoints", "intents", "branches", "impact?scope=uncommitted"] {
        let (status, body) = get(&format!("/api/{endpoint}"), &host, Some(&token));
        assert_eq!(status, 200, "{endpoint}: {body}");
    }
    // Nothing the viewer did changed the repository.
    env.assert_git_clean_and_valid();
}

/// Git with a non-interactive editor (for `git rebase --continue`, `git commit --no-edit`, …).
fn git_quiet(env: &Env, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new("git");
    env.apply(&mut cmd);
    cmd.env("GIT_EDITOR", "true").env("GIT_SEQUENCE_EDITOR", "true").current_dir(&env.repo).args(args).output().unwrap()
}

/// topic: two commits on top of base, the first conflicting with main's change to a.txt.
fn diverged_topic(env: &Env) -> String {
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("a.txt", "topic\n", "topic change");
    env.commit_file("b.txt", "b\n", "add b");
    let topic = env.git(&["rev-parse", "HEAD"]).trim().to_owned();
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "main\n", "main change");
    env.ok(&["switch", "topic"]);
    topic
}

#[test]
fn a_rebase_started_by_veyra_is_continued_or_aborted_by_git() {
    require_git!();
    let env = Env::new();
    let topic = diverged_topic(&env);

    env.veyra(&["rebase", "main"]).assert().failure().stdout(predicate::str::contains("CONFLICT"));
    assert!(env.repo.join(".git/rebase-merge/head-name").exists(), "Git's own state directory");
    assert!(env.git(&["status"]).contains("rebas"), "git status knows a rebase is in progress");

    // Git aborts it…
    let out = git_quiet(&env, &["rebase", "--abort"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.git(&["rev-parse", "HEAD"]).trim(), topic);
    assert_eq!(env.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "topic");

    // …or continues it after the conflict is resolved.
    env.veyra(&["rebase", "main"]).assert().failure();
    env.write("a.txt", "main and topic\n");
    env.git(&["add", "a.txt"]);
    let out = git_quiet(&env, &["rebase", "--continue"]);
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.git(&["log", "--format=%s", "-3"]), "add b\ntopic change\nmain change\n");
    assert_eq!(env.git(&["log", "-1", "--format=%an", "HEAD~1"]).trim(), "Test User");
    assert_eq!(env.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "topic");
    assert!(!env.repo.join(".git/rebase-merge").exists());
    env.assert_git_clean_and_valid();
    assert!(!env.ok(&["status"]).contains("rebasing"));
}

#[test]
fn a_rebase_started_by_git_is_continued_or_aborted_by_veyra() {
    require_git!();
    let env = Env::new();
    let topic = diverged_topic(&env);

    let out = git_quiet(&env, &["rebase", "main"]);
    assert!(!out.status.success(), "expected a conflict");
    assert!(env.ok(&["status"]).contains("rebasing onto"), "veyra status sees Git's rebase");

    env.ok(&["rebase", "--abort"]);
    assert_eq!(env.git(&["rev-parse", "HEAD"]).trim(), topic);
    assert!(!env.repo.join(".git/rebase-merge").exists());

    assert!(!git_quiet(&env, &["rebase", "main"]).status.success());
    env.write("a.txt", "main and topic\n");
    env.ok(&["add", "a.txt"]);
    let out = env.ok(&["rebase", "--continue"]);
    assert!(out.contains("Successfully rebased"), "{out}");
    assert_eq!(
        env.git(&["log", "--format=%s|%an", "-3"]),
        "add b|Test User\ntopic change|Test User\nmain change|Test User\n"
    );
    assert_eq!(env.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "topic");
    env.assert_git_clean_and_valid();

    // An interactive Git rebase stopped at `edit` continues in Veyra too.
    let mut cmd = Command::new("git");
    env.apply(&mut cmd);
    let out = cmd
        .env("GIT_SEQUENCE_EDITOR", "sed -i -e '1s/^pick/edit/'")
        .current_dir(&env.repo)
        .args(["rebase", "-i", "HEAD~2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    env.write("c.txt", "c\n");
    env.ok(&["add", "c.txt"]);
    env.ok(&["rebase", "--continue"]);
    assert_eq!(
        env.git(&["show", "--name-only", "--format=%s", "HEAD~1"])
            .lines()
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>(),
        vec!["topic change", "a.txt", "c.txt"]
    );
    env.assert_git_clean_and_valid();
}

#[test]
fn bisect_sessions_are_shared_with_git() {
    require_git!();
    let env = Env::new();
    for i in 0..10 {
        let content = if i >= 6 { format!("broken {i}\n") } else { format!("fine {i}\n") };
        env.commit_file("state.txt", &content, &format!("c{i}"));
    }
    let first_bad = env.git(&["rev-parse", "HEAD~3"]).trim().to_owned();
    // Veyra starts; Git sees the session and finishes it.
    env.ok(&["bisect", "start", "HEAD", "HEAD~9"]);
    assert!(env.git(&["bisect", "log"]).contains("git bisect bad"));
    for _ in 0..6 {
        let content = env.read("state.txt").unwrap();
        let verdict = if content.starts_with("fine") { "good" } else { "bad" };
        let out = git_quiet(&env, &["bisect", verdict]);
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if text.contains("is the first bad commit") {
            assert!(text.contains(&first_bad), "{text}");
            break;
        }
    }
    git_quiet(&env, &["bisect", "reset"]);
    assert_eq!(env.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "main");

    // Git starts; Veyra finishes and resets.
    assert!(git_quiet(&env, &["bisect", "start", "HEAD", "HEAD~9"]).status.success());
    let out = env.ok(&["bisect", "run", "grep", "-q", "fine", "state.txt"]);
    assert!(out.contains("is the first bad commit") && out.contains(&first_bad), "{out}");
    env.ok(&["bisect", "reset"]);
    assert_eq!(env.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "main");
    assert!(env.git(&["for-each-ref", "refs/bisect"]).is_empty());
    assert!(!env.repo.join(".git/BISECT_START").exists());
}

/// main and topic both change a.txt; topic also adds b.txt in a second commit.
fn pick_fixture(env: &Env) {
    env.commit_file("a.txt", "base\n", "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("a.txt", "t1\n", "t1");
    env.commit_file("b.txt", "t2\n", "t2");
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "m\n", "m");
}

#[test]
fn multi_commit_cherry_picks_are_shared_with_git() {
    require_git!();
    let env = Env::new();
    pick_fixture(&env);
    let main = env.git(&["rev-parse", "HEAD"]).trim().to_owned();

    // Veyra stops on the first commit; the second is remembered in Git's sequencer.
    let out = env.veyra(&["cherry-pick", "topic~1", "topic"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 commit still to apply"));
    assert!(env.repo.join(".git/sequencer/todo").exists());
    env.write("a.txt", "resolved\n");
    env.git(&["add", "a.txt"]);
    let out = git_quiet(&env, &["cherry-pick", "--continue"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.git(&["log", "--format=%s", "-3"]), "t2\nt1\nm\n", "git applied the rest");

    // Git stops; Veyra continues with the rest.
    git_quiet(&env, &["reset", "--hard", &main]);
    assert!(!git_quiet(&env, &["cherry-pick", "topic~1", "topic"]).status.success());
    env.write("a.txt", "resolved\n");
    env.ok(&["add", "a.txt"]);
    env.ok(&["cherry-pick", "--continue"]);
    assert_eq!(env.git(&["log", "--format=%s", "-3"]), "t2\nt1\nm\n");
    assert!(!env.repo.join(".git/sequencer").exists());
    env.assert_git_clean_and_valid();

    // --skip drops the stopped commit and applies the rest; --abort rewinds the whole sequence.
    git_quiet(&env, &["reset", "--hard", &main]);
    env.veyra(&["cherry-pick", "topic~1", "topic"]).assert().failure();
    env.ok(&["cherry-pick", "--skip"]);
    assert_eq!(env.git(&["log", "--format=%s", "-2"]), "t2\nm\n");
    git_quiet(&env, &["reset", "--hard", &main]);
    assert!(!git_quiet(&env, &["cherry-pick", "topic~1", "topic"]).status.success());
    env.ok(&["cherry-pick", "--abort"]);
    assert_eq!(env.git(&["rev-parse", "HEAD"]).trim(), main);
    env.assert_git_clean_and_valid();
}

/// A source repository with history in two directories, served over file:// with filters allowed.
fn filterable_source(env: &Env) -> (PathBuf, String) {
    let src = env.home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    git_in(env, &src, &["init", "-q", "-b", "main"]);
    git_in(env, &src, &["config", "uploadpack.allowFilter", "true"]);
    git_in(env, &src, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    for i in 1..=4 {
        std::fs::create_dir_all(src.join("app")).unwrap();
        std::fs::create_dir_all(src.join("docs")).unwrap();
        std::fs::write(src.join("app/main.txt"), format!("app {i}\n")).unwrap();
        std::fs::write(src.join("docs/guide.txt"), format!("docs {i}\n")).unwrap();
        git_in(env, &src, &["add", "."]);
        git_in(env, &src, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    let url = format!("file://{}", src.to_string_lossy().replace('\\', "/"));
    (src, url)
}

/// Is `rev` present locally, without letting Git fetch it on demand?
fn has_object_locally(env: &Env, dir: &std::path::Path, rev: &str) -> bool {
    let mut cmd = Command::new("git");
    env.apply(&mut cmd);
    cmd.env("GIT_NO_LAZY_FETCH", "1").current_dir(dir).args(["cat-file", "-e", rev]).output().unwrap().status.success()
}

#[test]
fn shallow_clones_and_unshallow() {
    require_git!();
    let env = Env::new();
    let (_src, url) = filterable_source(&env);
    let dest = env.home.join("shallow");
    let out = env.ok(&["clone", "--depth", "2", &url, dest.to_str().unwrap()]);
    assert!(out.contains("Shallow clone"), "{out}");
    assert_eq!(git_in(&env, &dest, &["rev-list", "--count", "HEAD"]).trim(), "2");
    assert!(dest.join(".git/shallow").exists());
    let mut log = env.veyra(&["log", "--oneline"]);
    let log = String::from_utf8_lossy(&log.current_dir(&dest).output().unwrap().stdout).to_string();
    assert_eq!(log.lines().count(), 2, "{log}");
    git_in(&env, &dest, &["fsck", "--no-dangling"]);

    let mut unshallow = env.veyra(&["fetch", "--unshallow"]);
    assert!(unshallow.current_dir(&dest).output().unwrap().status.success());
    assert_eq!(git_in(&env, &dest, &["rev-list", "--count", "HEAD"]).trim(), "4");
}

#[test]
fn partial_clones_fetch_missing_objects_on_demand() {
    require_git!();
    let env = Env::new();
    let (src, url) = filterable_source(&env);
    let dest = env.home.join("partial");
    let out = env.ok(&["clone", "--filter=blob:none", &url, dest.to_str().unwrap()]);
    assert!(out.contains("Partial clone"), "{out}");
    assert_eq!(git_in(&env, &dest, &["config", "remote.origin.promisor"]).trim(), "true");
    assert_eq!(std::fs::read_to_string(dest.join("app/main.txt")).unwrap(), "app 4\n");
    assert!(has_object_locally(&env, &dest, "HEAD:app/main.txt"));
    assert!(!has_object_locally(&env, &dest, "HEAD~1:app/main.txt"), "older blobs were left out");

    // Veyra fetches what it needs: a diff against the previous commit.
    let veyra = |args: &[&str]| {
        let out = env.veyra(args).current_dir(&dest).output().unwrap();
        assert!(out.status.success(), "veyra {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let diff = veyra(&["diff", "HEAD~1", "HEAD"]);
    assert!(diff.contains("-app 3") && diff.contains("+app 4"), "{diff}");
    assert!(has_object_locally(&env, &dest, "HEAD~1:app/main.txt"));

    // Fetching new history keeps the clone partial; backfill downloads what HEAD needs.
    std::fs::write(src.join("docs/guide.txt"), "docs 5\n").unwrap();
    git_in(&env, &src, &["commit", "-q", "-am", "c5"]);
    veyra(&["fetch"]);
    assert!(!has_object_locally(&env, &dest, "origin/main:docs/guide.txt"), "the filter still applies");
    veyra(&["pull"]);
    assert_eq!(std::fs::read_to_string(dest.join("docs/guide.txt")).unwrap(), "docs 5\n");
    git_in(&env, &dest, &["fsck", "--no-dangling"]);
    assert_eq!(git_in(&env, &dest, &["status", "--porcelain"]), "");
}

#[test]
fn partial_and_sparse_clone_downloads_only_the_scope() {
    require_git!();
    let env = Env::new();
    let (_src, url) = filterable_source(&env);
    let dest = env.home.join("scoped");
    env.ok(&["clone", "--filter=blob:none", "--sparse", "app", &url, dest.to_str().unwrap()]);
    assert!(dest.join("app/main.txt").exists());
    assert!(!dest.join("docs").exists());
    assert!(has_object_locally(&env, &dest, "HEAD:app/main.txt"));
    assert!(!has_object_locally(&env, &dest, "HEAD:docs/guide.txt"), "blobs outside the scope stay on the server");
    // Widening the scope fetches the newly needed blobs.
    let out = env.veyra(&["sparse", "add", "docs"]).current_dir(&dest).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(dest.join("docs/guide.txt")).unwrap(), "docs 4\n");
}

/// main and topic change the same line of a.txt.
fn conflicting_branches(env: &Env) {
    env.commit_file("a.txt", "one\nbase\nthree\n", "base");
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("a.txt", "one\ntopic\nthree\n", "topic");
    env.ok(&["switch", "main"]);
    env.commit_file("a.txt", "one\nmain\nthree\n", "main");
}

#[test]
fn merge_strategy_ours_and_favour_options() {
    require_git!();
    let env = Env::new();
    conflicting_branches(&env);
    let main_tree = env.git(&["rev-parse", "HEAD^{tree}"]);
    env.ok(&["merge", "-s", "ours", "topic"]);
    assert_eq!(env.git(&["rev-parse", "HEAD^{tree}"]), main_tree, "-s ours keeps our tree");
    assert_eq!(env.git(&["log", "-1", "--format=%p"]).split_whitespace().count(), 2);
    env.ok(&["undo"]);

    env.ok(&["merge", "-X", "theirs", "topic"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("one\ntopic\nthree\n"));
    env.ok(&["undo"]);
    env.ok(&["merge", "-X", "ours", "topic"]);
    assert_eq!(env.read("a.txt").as_deref(), Some("one\nmain\nthree\n"));
    env.assert_git_clean_and_valid();
    env.veyra(&["merge", "-s", "frobnicate", "topic"]).assert().failure();
}

#[test]
fn octopus_merges_several_branches_or_nothing() {
    require_git!();
    let env = Env::new();
    env.commit_file("base.txt", "base\n", "base");
    for b in ["b1", "b2", "b3"] {
        env.ok(&["switch", "-c", b, "main"]);
        env.commit_file(&format!("{b}.txt"), "x\n", b);
    }
    env.ok(&["switch", "main"]);
    let out = env.ok(&["merge", "b1", "b2", "b3"]);
    assert!(out.contains("Merge"), "{out}");
    assert_eq!(env.git(&["log", "-1", "--format=%p"]).split_whitespace().count(), 4);
    assert_eq!(env.git(&["log", "-1", "--format=%s"]).trim(), "Merge branches 'b1', 'b2' and 'b3'");
    for b in ["b1", "b2", "b3"] {
        assert!(env.read(&format!("{b}.txt")).is_some());
    }
    env.assert_git_clean_and_valid();

    // A conflicting head stops the whole octopus before anything changes.
    let before = env.git(&["rev-parse", "HEAD"]);
    for (b, content) in [("c1", "one\n"), ("c2", "two\n")] {
        env.ok(&["switch", "-c", b, "main"]);
        env.commit_file("same.txt", content, b);
    }
    env.ok(&["switch", "main"]);
    env.veyra(&["merge", "c1", "c2"]).assert().failure().stderr(predicate::str::contains("octopus"));
    assert_eq!(env.git(&["rev-parse", "HEAD"]), before);
    env.assert_git_clean_and_valid();
}

#[test]
fn subtree_merges_into_a_directory() {
    require_git!();
    let env = Env::new();
    env.commit_file("app.txt", "app\n", "app");
    // An unrelated project history on branch `lib`.
    env.git(&["switch", "-q", "--orphan", "lib"]); // empties the index and removes tracked files
    env.write("lib.txt", "lib 1\n");
    env.write("util/helper.txt", "help\n");
    env.git(&["add", "."]);
    env.git(&["commit", "-q", "-m", "lib 1"]);
    env.git(&["switch", "-q", "main"]);

    // First merge: named directory (unrelated histories).
    env.ok(&["merge", "-X", "subtree=vendor/lib", "lib"]);
    assert_eq!(env.read("vendor/lib/lib.txt").as_deref(), Some("lib 1\n"));
    assert_eq!(env.read("app.txt").as_deref(), Some("app\n"));
    assert!(env.read("lib.txt").is_none(), "nothing lands at the top level");

    // Later merges find the directory on their own.
    env.git(&["switch", "-q", "lib"]);
    env.write("lib.txt", "lib 2\n");
    env.git(&["commit", "-q", "-am", "lib 2"]);
    env.git(&["switch", "-q", "main"]);
    env.commit_file("app.txt", "app 2\n", "app 2");
    env.ok(&["merge", "-s", "subtree", "lib"]);
    assert_eq!(env.read("vendor/lib/lib.txt").as_deref(), Some("lib 2\n"));
    assert_eq!(env.read("app.txt").as_deref(), Some("app 2\n"));
    env.assert_git_clean_and_valid();
}

fn worktree_files(dir: &std::path::Path) -> Vec<String> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn non_cone_sparse_patterns_agree_with_git() {
    require_git!();
    let env = Env::new();
    for f in [
        "README.md",
        "top.txt",
        "docs/guide.md",
        "docs/img/logo.png",
        "docs/drafts/wip.md",
        "src/a/test_x.rs",
        "src/a/x.rs",
        "notes/n.md",
    ] {
        env.write(f, "x\n");
    }
    env.ok(&["add", "."]);
    env.ok(&["commit", "-m", "files"]);

    let patterns = ["/*", "!/*/", "/docs/", "!/docs/drafts/", "*.md", "!*.png", "src/**/test_*.rs"];
    let mut args = vec!["sparse", "set", "--no-cone"];
    args.extend(patterns);
    env.ok(&args);
    let ours = worktree_files(&env.repo);
    assert_eq!(
        ours,
        vec!["README.md", "docs/drafts/wip.md", "docs/guide.md", "notes/n.md", "src/a/test_x.rs", "top.txt"]
    );
    assert_eq!(env.git(&["config", "core.sparseCheckoutCone"]).trim(), "false");
    // Git applying the same patterns changes nothing.
    env.git(&["sparse-checkout", "reapply"]);
    assert_eq!(worktree_files(&env.repo), ours);
    assert_eq!(env.git(&["status", "--porcelain"]), "");

    // Patterns set by Git are honoured by Veyra (checkout of another commit keeps them).
    let mut gitargs = vec!["sparse-checkout", "set", "--no-cone"];
    gitargs.extend(["/*", "!/*/", "/src/"]);
    env.git(&gitargs);
    let git_view = worktree_files(&env.repo);
    env.commit_file("src/a/new.rs", "n\n", "more");
    env.write("docs/guide.md", "changed but hidden\n");
    assert!(env.ok(&["sparse", "list"]).contains("/src/"));
    env.ok(&["switch", "-c", "other"]);
    let mut expected = git_view.clone();
    expected.push("src/a/new.rs".into());
    expected.sort();
    expected.retain(|f| f != "docs/guide.md");
    let mut now = worktree_files(&env.repo);
    now.retain(|f| f != "docs/guide.md");
    assert_eq!(now, expected);
}

fn doctor_objects_check(env: &Env, dir: &std::path::Path) -> (bool, serde_json::Value) {
    let out = env.veyra(&["doctor", "--full", "--json"]).current_dir(dir).output().unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let check = doc["checks"].as_array().unwrap().iter().find(|c| c["name"] == "Objects").unwrap().clone();
    (out.status.success(), check)
}

/// Make a stored object file writable (Git stores them read-only).
fn writable(path: &std::path::Path) {
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).unwrap();
}

#[test]
fn doctor_full_finds_corrupt_and_missing_objects() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "alpha\n", "one");
    env.commit_file("b.txt", "beta\n", "two");
    env.git(&["gc", "-q"]);
    env.commit_file("c.txt", "gamma\n", "three");
    let (ok, check) = doctor_objects_check(&env, &env.repo);
    assert!(ok, "{check}");
    assert_eq!(check["status"], "pass", "{check}");
    // (`git gc` may also write a cruft pack.)
    let evidence = check["evidence"].as_str().unwrap().to_owned();
    assert!(!evidence.contains(" 0 packs") && evidence.contains("verified"), "{check}");

    // A loose object whose content hashes to another id.
    let blob = env.git(&["rev-parse", "HEAD:c.txt"]).trim().to_owned();
    let commit = env.git(&["rev-parse", "HEAD"]).trim().to_owned();
    let loose = |id: &str| env.repo.join(".git/objects").join(&id[..2]).join(&id[2..]);
    let original = std::fs::read(loose(&blob)).unwrap();
    writable(&loose(&blob));
    std::fs::copy(loose(&commit), loose(&blob)).unwrap();
    let (ok, check) = doctor_objects_check(&env, &env.repo);
    assert!(!ok);
    assert_eq!(check["status"], "fail");
    let evidence = check["evidence"].as_str().unwrap();
    assert!(evidence.contains(&format!("corrupt {blob}: content hashes to {commit}")), "{evidence}");
    let mut fsck = Command::new("git");
    env.apply(&mut fsck);
    assert!(!fsck.current_dir(&env.repo).args(["fsck", "--full"]).output().unwrap().status.success());

    // A reachable object that's gone.
    std::fs::remove_file(loose(&blob)).unwrap();
    let (ok, check) = doctor_objects_check(&env, &env.repo);
    assert!(!ok);
    assert!(check["evidence"].as_str().unwrap().contains(&format!("missing {blob}")), "{check}");
    std::fs::write(loose(&blob), &original).unwrap();
    assert!(doctor_objects_check(&env, &env.repo).0);

    // A damaged pack.
    let pack = std::fs::read_dir(env.repo.join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .unwrap();
    let mut bytes = std::fs::read(&pack).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    writable(&pack);
    std::fs::write(&pack, &bytes).unwrap();
    let (ok, check) = doctor_objects_check(&env, &env.repo);
    assert!(!ok);
    assert!(check["evidence"].as_str().unwrap().contains("bad pack"), "{check}");
}

#[test]
fn doctor_full_accepts_objects_a_partial_clone_left_behind() {
    require_git!();
    let env = Env::new();
    let (_src, url) = filterable_source(&env);
    let dest = env.home.join("partial");
    env.ok(&["clone", "--filter=blob:none", &url, dest.to_str().unwrap()]);
    let (ok, check) = doctor_objects_check(&env, &dest);
    assert!(ok, "{check}");
    assert!(check["evidence"].as_str().unwrap().contains("on the promisor remote"), "{check}");

    // A tree filter leaves older trees behind too; that's not a broken repository either.
    let trees = env.home.join("treeless");
    env.ok(&["clone", "--filter=tree:0", &url, trees.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(trees.join("docs/guide.txt")).unwrap(), "docs 4\n");
    let (ok, check) = doctor_objects_check(&env, &trees);
    assert!(ok, "{check}");
    // Older trees arrive when history needs them.
    assert!(!has_object_locally(&env, &trees, "HEAD~3^{tree}"));
    let diff = env.veyra(&["diff", "--name-only", "HEAD~3", "HEAD"]).current_dir(&trees).output().unwrap();
    assert!(diff.status.success(), "{}", String::from_utf8_lossy(&diff.stderr));
    assert_eq!(String::from_utf8_lossy(&diff.stdout), "app/main.txt\ndocs/guide.txt\n");
    git_in(&env, &trees, &["fsck", "--no-dangling"]);
}

#[test]
fn reftable_repositories_are_refused_with_the_way_out() {
    require_git!();
    let env = Env::new();
    let dir = env.home.join("reftable");
    let mut init = Command::new("git");
    env.apply(&mut init);
    let made = init.args(["init", "-q", "--ref-format=reftable"]).arg(&dir).output().unwrap();
    if !made.status.success() {
        eprintln!("skipping: this git can't create reftable repositories");
        return;
    }
    env.veyra(&["status"]).current_dir(&dir).assert().failure().stderr(
        predicate::str::contains("reftable").and(predicate::str::contains("git refs migrate --ref-format=files")),
    );
    git_in(&env, &dir, &["refs", "migrate", "--ref-format=files"]);
    env.veyra(&["status"]).current_dir(&dir).assert().success();
}

fn git_lfs_available() -> bool {
    Command::new("git").args(["lfs", "version"]).output().is_ok_and(|o| o.status.success())
}

#[test]
fn git_lfs_files_round_trip_through_veyra() {
    require_git!();
    if !git_lfs_available() {
        eprintln!("skipping: git-lfs not installed");
        return;
    }
    let env = Env::new();
    // Filters go in the (test) global config so the clone below has them too; hooks go here.
    env.git(&["lfs", "install"]);
    env.git(&["lfs", "track", "*.bin"]);
    env.ok(&["add", ".gitattributes"]);
    env.write("asset.bin", "binary payload\n");
    env.ok(&["add", "asset.bin"]);
    env.ok(&["commit", "-m", "lfs asset"]);
    let stored = env.git(&["cat-file", "-p", "HEAD:asset.bin"]);
    assert!(stored.starts_with("version https://git-lfs.github.com/spec/v1"), "stored as a pointer: {stored}");
    assert!(env.git(&["lfs", "ls-files"]).contains("asset.bin"));
    assert_eq!(env.ok(&["status", "--porcelain"]), "");
    env.assert_git_clean_and_valid();

    // Veyra's push runs the pre-push hook, which uploads the content.
    let bare = env.home.join("remote.git");
    git_in(&env, &env.home, &["init", "-q", "--bare", bare.to_str().unwrap()]);
    env.ok(&["remote", "add", "origin", bare.to_str().unwrap()]);
    env.ok(&["push", "-u", "origin", "main"]);
    assert!(bare.join("lfs/objects").is_dir(), "the pre-push hook uploaded the LFS object");

    // A fresh clone checks the real content out through the smudge filter.
    let clone = env.home.join("clone");
    env.ok(&["clone", bare.to_str().unwrap(), clone.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(clone.join("asset.bin")).unwrap(), "binary payload\n");
    assert_eq!(git_in(&env, &clone, &["status", "--porcelain"]), "");
}

#[test]
fn rewrite_hooks_run_like_git() {
    require_git!();
    let env = Env::new();
    env.commit_file("a.txt", "a\n", "c1");
    let old = env.git(&["rev-parse", "HEAD"]).trim().to_owned();
    install_hook(&env, "post-rewrite", "echo \"$1\" > .git/rewrite-args\ncat > .git/rewrite-input");
    env.ok(&["commit", "--amend", "-m", "c1 amended"]);
    let new = env.git(&["rev-parse", "HEAD"]).trim().to_owned();
    assert_eq!(std::fs::read_to_string(env.repo.join(".git/rewrite-args")).unwrap().trim(), "amend");
    assert_eq!(std::fs::read_to_string(env.repo.join(".git/rewrite-input")).unwrap(), format!("{old} {new}\n"));

    // pre-rebase can veto a rebase before anything changes.
    env.ok(&["switch", "-c", "topic"]);
    env.commit_file("t.txt", "t\n", "t");
    env.ok(&["switch", "main"]);
    env.commit_file("m.txt", "m\n", "m");
    env.ok(&["switch", "topic"]);
    let before = env.git(&["rev-parse", "HEAD"]);
    install_hook(&env, "pre-rebase", "echo \"$1\" > .git/pre-rebase-args\nexit 1");
    env.veyra(&["rebase", "main"]).assert().failure().stderr(predicate::str::contains("pre-rebase hook failed"));
    assert_eq!(env.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(std::fs::read_to_string(env.repo.join(".git/pre-rebase-args")).unwrap().trim(), "main");
    install_hook(&env, "pre-rebase", "exit 0");
    env.ok(&["rebase", "main"]);
    env.assert_git_clean_and_valid();
}

#[test]
fn semantic_diff_and_impact_cover_more_languages() {
    require_git!();
    let env = Env::new();
    env.write("include/geo.h", "#pragma once\nint area(int w, int h);\n");
    env.write("src/geo.cpp", "#include \"geo.h\"\nint area(int w, int h) { return w * h; }\nint perimeter(int w, int h) { return 2 * (w + h); }\n");
    env.write("src/main.c", "#include \"../include/geo.h\"\nint main(void) { return area(1, 2); }\n");
    env.write("lib/billing.rb", "class Invoice\n  def total(a)\n    a\n  end\n\n  def due\n    1\n  end\nend\n");
    env.ok(&["add", "."]);
    env.ok(&["commit", "-m", "base"]);

    env.write("src/geo.cpp", "#include \"geo.h\"\nint area(int w, int h) { return w * h * 1; }\nint perimeter(int w, int h) { return 2 * (w + h); }\n");
    env.write(
        "lib/billing.rb",
        "class Invoice\n  def total(a, tax)\n    a + tax\n  end\n\n  def due\n    1\n  end\nend\n",
    );
    let out = env.ok(&["diff", "--semantic"]);
    assert!(out.contains("function area"), "{out}");
    assert!(!out.contains("perimeter"), "{out}");
    assert!(out.contains("Invoice::total"), "{out}");
    assert!(!out.contains("due"), "{out}");
    let json: serde_json::Value = serde_json::from_str(&env.ok(&["diff", "--functions", "--json"])).unwrap();
    let languages: Vec<&str> = json.as_array().unwrap().iter().filter_map(|f| f["language"].as_str()).collect();
    assert!(languages.contains(&"C++") && languages.contains(&"Ruby"), "{json}");
    env.ok(&["commit", "-am", "tweak"]);

    // A header change reaches every file that includes it.
    env.write("include/geo.h", "#pragma once\nint area(int w, int h, int scale);\n");
    let out = env.ok(&["impact"]);
    assert!(out.contains("src/main.c") && out.contains("src/geo.cpp"), "{out}");
    assert!(out.contains("imports include/geo.h"), "{out}");
}

#[test]
fn split_commit_can_split_one_file_by_hunk() {
    require_git!();
    let env = Env::new();
    fake_ai(&env);
    let lines: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    env.commit_file("a.txt", &lines, "init");
    let changed = lines.replace("line 2\n", "line two\n").replace("line 19\n", "line nineteen\n");
    env.write("a.txt", &changed);
    env.write("b.txt", "b\n");
    env.ok(&["add", "a.txt", "b.txt"]);
    // This plan sends the second hunk of a.txt with b.txt, and the first hunk on its own.
    let script = env.home.join("fake-ai.sh");
    let s = std::fs::read_to_string(&script).unwrap();
    let plan = r#"{"commits":[{"message":"fix: line two","changes":["a.txt#1"]},{"message":"feat: b and nineteen","changes":["b.txt","a.txt#2"]}]}"#;
    let old_plan = s.lines().find(|l| l.contains("\"commits\"")).unwrap().to_owned();
    std::fs::write(&script, s.replace(&old_plan, &format!("' '{plan}' ;;"))).unwrap();

    let out = env.ok(&["commit", "--split", "--yes"]);
    assert!(out.contains("Proposed: 2 commits") && out.contains("a.txt#2"), "{out}");
    assert_eq!(env.git(&["log", "--format=%s", "-2"]), "feat: b and nineteen\nfix: line two\n");
    // The first commit has only the first hunk; the second completes the file.
    let first = env.git(&["show", "HEAD~1:a.txt"]);
    assert_eq!(first, lines.replace("line 2\n", "line two\n"));
    assert_eq!(env.git(&["show", "--name-only", "--format=", "HEAD~1"]).trim(), "a.txt");
    assert_eq!(env.git(&["show", "HEAD:a.txt"]), changed);
    env.assert_git_clean_and_valid();
}

#[test]
fn analyzer_plugins_run_only_once_trusted() {
    require_git!();
    let env = Env::new();
    env.write(
        "tools/plugin.sh",
        "#!/bin/sh\ninput=$(cat)\nprintf '%s' \"$input\" > .git/plugin-input\ncase \"$input\" in\n  *TODO*) echo '{\"findings\":[{\"severity\":\"high\",\"rule\":\"todo\",\"path\":\"src/a.txt\",\"line\":2,\"message\":\"No TODOs in src\"}]}' ;;\n  *) echo '{\"findings\":[]}' ;;\nesac\n",
    );
    env.write(
        ".veyra/analyzers.toml",
        "[[plugin]]\nid = \"todo-check\"\ncommand = [\"sh\", \"tools/plugin.sh\"]\npaths = [\"src/**\"]\n",
    );
    env.write("src/a.txt", "one\n");
    env.ok(&["add", "."]);
    env.ok(&["commit", "-m", "base"]);
    env.write("src/a.txt", "one\nTODO two\n");
    env.write("docs/b.txt", "TODO elsewhere\n");
    env.ok(&["add", "."]);

    // Committed, but not trusted here: it doesn't run, and says how to trust it.
    let out = env.veyra(&["analyze", "--fail-on", "high"]).output().unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("isn't trusted here") && stderr.contains("--trust-plugin todo-check"), "{stderr}");
    assert!(!env.repo.join(".git/plugin-input").exists(), "the plugin never ran");

    // Trusted: it runs on the files it asked for, and its findings count.
    env.veyra(&["analyze", "--trust-plugin", "todo-check", "--fail-on", "high"]).assert().failure();
    let input: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.repo.join(".git/plugin-input")).unwrap()).unwrap();
    assert_eq!(input["version"], 1);
    let sent: Vec<&str> = input["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(sent, vec!["src/a.txt"], "only files matching its paths");
    let json: serde_json::Value = serde_json::from_str(&env.ok(&["analyze", "--json"])).unwrap();
    assert_eq!(json[0]["rule"], "todo-check/todo", "{json}");
    assert_eq!(json[0]["area"], "plugin");

    // A changed definition (say, from a pull) is not trusted until reviewed again.
    env.write(
        ".veyra/analyzers.toml",
        "[[plugin]]\nid = \"todo-check\"\ncommand = [\"sh\", \"tools/plugin.sh\", \"--more\"]\npaths = [\"src/**\"]\n",
    );
    let out = env.veyra(&["analyze", "--json"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[]");
    assert!(String::from_utf8_lossy(&out.stderr).contains("isn't trusted here"));
    // And a broken trusted plugin fails --fail-on instead of passing silently.
    env.write("tools/plugin.sh", "#!/bin/sh\nexit 3\n");
    env.ok(&["analyze", "--trust-plugin", "todo-check", "--no-plugins"]);
    let out = env.veyra(&["analyze", "--fail-on", "high"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("plugin todo-check failed (exit code 3)"));
}
