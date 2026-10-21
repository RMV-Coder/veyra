//! Performance budgets (DESIGN.md / ROADMAP Phase 3). Opt-in, because they create a large
//! repository:
//!
//! ```text
//! cargo test --release -p veyra-cli --test perf -- --ignored --nocapture
//! ```
//!
//! Budgets are for release builds on a typical developer laptop; scale them with
//! `VEYRA_PERF_SCALE=2` on slow machines (or debug builds, which are ~10× slower).

use std::process::Command;
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;

const FILES: usize = 10_000;

fn budget(base_ms: u64) -> Duration {
    let scale: f64 = std::env::var("VEYRA_PERF_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    Duration::from_millis((base_ms as f64 * scale) as u64)
}

fn timed(dir: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Duration {
    let mut cmd = Command::cargo_bin("veyra").unwrap();
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(dir)
        .args(args);
    let start = Instant::now();
    let out = cmd.output().unwrap();
    let took = start.elapsed();
    assert!(out.status.success(), "veyra {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    took
}

#[test]
#[ignore = "creates a 10k-file repository; run explicitly"]
fn budgets_for_a_ten_thousand_file_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = Perf\n\temail = perf@example.com\n[core]\n\tautocrlf = false\n",
    )
    .unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    for i in 0..FILES {
        let dir = repo.join(format!("src/module{}/sub{}", i % 100, i % 7));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("file{i}.rs")), format!("// file {i}\npub fn f{i}() -> u32 {{ {i} }}\n"))
            .unwrap();
    }
    // Read every file once first: on Windows, the first read of a freshly created file pays for
    // an antivirus scan (~10 ms), which would measure the scanner rather than Veyra.
    for entry in walk(&repo.join("src")) {
        let _ = std::fs::read(entry);
    }
    timed(&repo, &home, &["init"]);
    let add = timed(&repo, &home, &["add", "-A"]);
    let commit = timed(&repo, &home, &["commit", "-m", "10k files"]);
    let status_clean = timed(&repo, &home, &["status", "--short"]);
    for i in 0..50 {
        let path = repo.join(format!("src/module{}/sub{}/file{i}.rs", i % 100, i % 7));
        std::fs::write(path, format!("// edited {i}\n")).unwrap();
    }
    let status_dirty = timed(&repo, &home, &["status", "--short"]);
    let checkpoint = timed(&repo, &home, &["checkpoint", "-m", "perf"]);

    println!(
        "add -A: {add:?}  commit: {commit:?}  status(clean): {status_clean:?}  status(50 edits): {status_dirty:?}  checkpoint: {checkpoint:?}"
    );
    assert!(status_clean < budget(1_000), "clean status took {status_clean:?}");
    assert!(status_dirty < budget(1_500), "dirty status took {status_dirty:?}");
    assert!(checkpoint < budget(2_500), "checkpoint took {checkpoint:?}");
    assert!(add < budget(10_000), "add -A took {add:?}");
    assert!(commit < budget(5_000), "commit took {commit:?}");
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p) } else { out.push(p) }
        }
    }
    out
}
