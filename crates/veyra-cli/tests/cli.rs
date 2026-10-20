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
