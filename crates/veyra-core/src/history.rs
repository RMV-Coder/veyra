//! History tools: `describe`, `shortlog`, `grep` and `bisect`.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use serde::{Deserialize, Serialize};

use crate::Repo;
use crate::checkout::SwitchTarget;

/// How `describe` names a commit.
#[derive(Debug, Clone, Default)]
pub struct DescribeOptions {
    /// Consider lightweight tags too, not only annotated ones.
    pub tags: bool,
    /// Always print `<tag>-<n>-g<abbrev>`, even on the tagged commit.
    pub long: bool,
    /// Fall back to the abbreviated id when no tag is found.
    pub always: bool,
}

/// Commits reachable from `tip` but not from any of `hidden`.
fn count_between(repo: &Repo, tip: ObjectId, hidden: &[ObjectId]) -> Result<usize> {
    Ok(repo.git.rev_walk([tip]).with_hidden(hidden.iter().copied()).all()?.count())
}

impl Repo {
    /// Name `commit` after the nearest tag: `v1.2.0`, or `v1.2.0-3-gabc1234` three commits later.
    pub fn describe(&self, commit: ObjectId, options: &DescribeOptions) -> Result<Option<String>> {
        // Tagged commits (annotated first, as Git prefers them).
        let mut tagged: HashMap<ObjectId, (bool, String)> = HashMap::new();
        for (name, id) in self.tags()? {
            let annotated = self.git.find_object(id)?.kind == gix::object::Kind::Tag;
            if !annotated && !options.tags {
                continue;
            }
            let Ok(target) = self.peel_to_commit(id) else { continue };
            let better = tagged.get(&target).is_none_or(|(a, n)| (annotated && !a) || (annotated == *a && name > *n));
            if better {
                tagged.insert(target, (annotated, name));
            }
        }
        // Walk back from the commit; keep the first few tags found and pick the closest.
        let mut candidates: Vec<(usize, String, ObjectId)> = Vec::new();
        let walk = self.git.rev_walk([commit]).all()?;
        for info in walk {
            let id = info?.id;
            if let Some((_, name)) = tagged.get(&id) {
                let distance = count_between(self, commit, &[id])?;
                candidates.push((distance, name.clone(), id));
                if distance == 0 || candidates.len() >= 10 {
                    break;
                }
            }
        }
        candidates.sort();
        let abbrev = crate::short(&commit);
        Ok(match candidates.first() {
            Some((0, name, _)) if !options.long => Some(name.clone()),
            Some((n, name, _)) => Some(format!("{name}-{n}-g{abbrev}")),
            None if options.always => Some(abbrev),
            None => None,
        })
    }

    /// Commits per author in `tip` minus `hidden`: `(author "Name <email>", subjects)`, most first.
    pub fn shortlog(&self, tip: ObjectId, hidden: &[ObjectId]) -> Result<Vec<(String, Vec<String>)>> {
        let mut by_author: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let walk = self.git.rev_walk([tip]).with_hidden(hidden.iter().copied()).all()?;
        for info in walk {
            let c = self.commit_info(info?.id)?;
            by_author
                .entry(format!("{} <{}>", c.author_name, c.author_email))
                .or_default()
                .push(c.summary().to_owned());
        }
        let mut out: Vec<_> = by_author.into_iter().collect();
        for (_, subjects) in &mut out {
            subjects.reverse(); // oldest first, like git
        }
        out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }
}

// ------------------------------------------------------------------------------------------
// grep

/// One match of `veyra grep`.
#[derive(Debug, Clone, Serialize)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Where `grep` looks.
pub enum GrepSource {
    /// Tracked files as they are on disk.
    Worktree,
    /// The staged versions.
    Index,
    /// A commit's tree.
    Tree(ObjectId),
}

impl Repo {
    /// Search tracked files for `pattern` (paths filtered by `spec`). Binary files are skipped.
    pub fn grep(
        &self,
        pattern: &regex::Regex,
        source: GrepSource,
        spec: &crate::worktree::Pathspec,
    ) -> Result<Vec<GrepMatch>> {
        let flat = match source {
            GrepSource::Tree(tree) => self.flat_from_tree(tree)?,
            _ => self.flat_index()?,
        };
        let mut out = Vec::new();
        for (path, entry) in &flat {
            if !spec.matches(path.as_ref()) || entry.kind == gix::objs::tree::EntryKind::Commit {
                continue;
            }
            let data = match source {
                GrepSource::Worktree => match std::fs::read(self.workdir()?.join(crate::repo::fs_path(path.as_ref())?))
                {
                    Ok(d) => d,
                    Err(_) => continue, // deleted or skip-worktree
                },
                _ => self.read_blob(entry.id)?,
            };
            if crate::diff::is_binary(&data) {
                continue;
            }
            for (i, line) in data.lines().enumerate() {
                let text = line.to_str_lossy();
                if pattern.is_match(&text) {
                    out.push(GrepMatch { path: path.to_string(), line: i + 1, text: text.into_owned() });
                }
            }
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------------------------------
// bisect

/// An in-progress bisection, stored in Git's own layout (`BISECT_START`, `BISECT_TERMS`,
/// `BISECT_LOG`, `refs/bisect/bad`, `refs/bisect/good-*`, `refs/bisect/skip-*`), so `git bisect`
/// and `veyra bisect` can take turns in the same session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BisectState {
    pub bad: Option<String>,
    pub good: Vec<String>,
    pub skip: Vec<String>,
    /// Where to go back to on `bisect reset`: a full branch name or a commit id.
    pub original: String,
    #[serde(default)]
    pub log: Vec<String>,
    /// The terms in use (`bad`/`good` unless the session was started with custom terms).
    #[serde(default)]
    pub terms: Option<(String, String)>,
}

/// What the next bisection step is.
#[derive(Debug, Clone)]
pub enum BisectStep {
    /// Mark more commits (no bad, or no good one yet).
    NeedMarks { bad: bool, good: bool },
    /// Test this commit next (it's checked out); `remaining` candidates, about `steps` steps left.
    Test { commit: ObjectId, remaining: usize, steps: u32 },
    /// The first bad commit was found.
    Found(ObjectId),
    /// Only skipped commits are left; the first bad one is among these.
    OnlySkipped(Vec<ObjectId>),
}

impl Repo {
    fn legacy_bisect_path(&self) -> std::path::PathBuf {
        self.veyra_dir().join("bisect.json")
    }

    fn bisect_file(&self, name: &str) -> std::path::PathBuf {
        self.git_dir().join(name)
    }

    /// The bad and good terms of the session (`bad`/`good`, or e.g. `new`/`old`).
    fn bisect_terms(&self) -> (String, String) {
        let text = std::fs::read_to_string(self.bisect_file("BISECT_TERMS")).unwrap_or_default();
        let mut lines = text.lines();
        match (lines.next(), lines.next()) {
            (Some(b), Some(g)) if !b.is_empty() && !g.is_empty() => (b.to_owned(), g.to_owned()),
            _ => ("bad".into(), "good".into()),
        }
    }

    pub fn bisect_state(&self) -> Result<Option<BisectState>> {
        let Ok(start) = std::fs::read_to_string(self.bisect_file("BISECT_START")) else {
            // A session started by Veyra < 1.3.
            return match std::fs::read_to_string(self.legacy_bisect_path()) {
                Ok(text) => Ok(Some(serde_json::from_str(&text).context("corrupt bisect state")?)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            };
        };
        let start = start.trim();
        // Git stores the branch's short name, or a commit id when HEAD was detached.
        let original = if start.len() >= 40 && start.bytes().all(|b| b.is_ascii_hexdigit()) {
            start.to_owned()
        } else {
            format!("refs/heads/{start}")
        };
        let (bad_term, good_term) = self.bisect_terms();
        let mut state = BisectState { original, ..Default::default() };
        for (name, id) in self.all_refs()? {
            let Some(rest) = name.strip_prefix("refs/bisect/") else { continue };
            if rest == bad_term {
                state.bad = Some(id.to_string());
            } else if rest.starts_with(&format!("{good_term}-")) {
                state.good.push(id.to_string());
            } else if rest.starts_with("skip-") {
                state.skip.push(id.to_string());
            }
        }
        state.log = std::fs::read_to_string(self.bisect_file("BISECT_LOG"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("git bisect "))
            .map(|l| l.trim_start_matches("git bisect ").to_owned())
            .collect();
        if (bad_term.as_str(), good_term.as_str()) != ("bad", "good") {
            state.terms = Some((bad_term, good_term));
        }
        Ok(Some(state))
    }

    fn bisect_log(&self, lines: &[String]) -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.bisect_file("BISECT_LOG"))?;
        for l in lines {
            writeln!(f, "{l}")?;
        }
        Ok(())
    }

    /// Move a session started by Veyra < 1.3 into Git's layout.
    fn migrate_legacy_bisect(&self, state: &BisectState) -> Result<()> {
        let start = if state.original.starts_with("refs/") {
            crate::refs::shorten(&state.original).to_owned()
        } else {
            state.original.clone()
        };
        std::fs::write(self.bisect_file("BISECT_START"), format!("{start}\n"))?;
        std::fs::write(self.bisect_file("BISECT_TERMS"), "bad\ngood\n")?;
        std::fs::write(self.bisect_file("BISECT_NAMES"), "\n")?;
        std::fs::write(self.bisect_file("BISECT_LOG"), "git bisect start\n")?;
        let id = |h: &String| ObjectId::from_hex(h.as_bytes());
        if let Some(b) = &state.bad {
            self.set_ref("refs/bisect/bad", id(b)?, None, "bisect bad")?;
        }
        for g in &state.good {
            self.set_ref(&format!("refs/bisect/good-{g}"), id(g)?, None, "bisect good")?;
        }
        for k in &state.skip {
            self.set_ref(&format!("refs/bisect/skip-{k}"), id(k)?, None, "bisect skip")?;
        }
        let _ = std::fs::remove_file(self.legacy_bisect_path());
        Ok(())
    }

    /// Start bisecting (remembering where HEAD is).
    pub fn bisect_start(&self) -> Result<BisectState> {
        if self.bisect_state()?.is_some() {
            bail!("a bisection is already in progress\n  hint: `veyra bisect reset` ends it");
        }
        let (original, start) = match self.head_state()? {
            crate::HeadState::Branch { name, .. } => {
                let full = name.to_string();
                let short = crate::refs::shorten(&full).to_owned();
                (full, short)
            }
            crate::HeadState::Detached(id) => (id.to_string(), id.to_string()),
        };
        std::fs::write(self.bisect_file("BISECT_START"), format!("{start}\n"))?;
        std::fs::write(self.bisect_file("BISECT_TERMS"), "bad\ngood\n")?;
        std::fs::write(self.bisect_file("BISECT_NAMES"), "\n")?;
        std::fs::write(self.bisect_file("BISECT_LOG"), "git bisect start\n")?;
        Ok(BisectState { original, log: vec!["start".into()], ..Default::default() })
    }

    /// Mark `commit` as `good`, `bad` or `skip`, then check out the next commit to test.
    pub fn bisect_mark(&self, mark: &str, commit: ObjectId) -> Result<BisectStep> {
        let mut state = self.bisect_state()?.context("no bisection in progress\n  hint: veyra bisect start")?;
        if !self.bisect_file("BISECT_START").exists() {
            self.migrate_legacy_bisect(&state)?;
        }
        let hex = commit.to_string();
        let (bad_term, good_term) = self.bisect_terms();
        let (term, reference) = match mark {
            "bad" => {
                state.bad = Some(hex.clone());
                (bad_term.clone(), format!("refs/bisect/{bad_term}"))
            }
            "good" => {
                state.good.push(hex.clone());
                (good_term.clone(), format!("refs/bisect/{good_term}-{hex}"))
            }
            "skip" => {
                state.skip.push(hex.clone());
                ("skip".to_owned(), format!("refs/bisect/skip-{hex}"))
            }
            other => bail!("unknown bisect mark '{other}'"),
        };
        self.set_ref(&reference, commit, None, &format!("bisect {term}"))?;
        let subject = self.commit_info(commit).map(|c| c.summary().to_owned()).unwrap_or_default();
        self.bisect_log(&[format!("# {term}: [{hex}] {subject}"), format!("git bisect {term} {hex}")])?;
        state.log.push(format!("{term} {hex}"));
        let step = self.bisect_next(&state)?;
        match &step {
            BisectStep::Test { commit, .. } => {
                std::fs::write(self.bisect_file("BISECT_EXPECTED_REV"), format!("{commit}\n"))?;
            }
            BisectStep::Found(c) => {
                let subject = self.commit_info(*c).map(|i| i.summary().to_owned()).unwrap_or_default();
                self.bisect_log(&[format!("# first {bad_term} commit: [{c}] {subject}")])?;
            }
            _ => {}
        }
        Ok(step)
    }

    fn bisect_next(&self, state: &BisectState) -> Result<BisectStep> {
        let (Some(bad), false) = (&state.bad, state.good.is_empty()) else {
            return Ok(BisectStep::NeedMarks { bad: state.bad.is_none(), good: state.good.is_empty() });
        };
        let bad = ObjectId::from_hex(bad.as_bytes())?;
        let good: Vec<ObjectId> =
            state.good.iter().map(|g| ObjectId::from_hex(g.as_bytes())).collect::<Result<_, _>>()?;
        let skip: HashSet<ObjectId> = state.skip.iter().filter_map(|s| ObjectId::from_hex(s.as_bytes()).ok()).collect();
        for g in &good {
            if !self.is_ancestor(*g, bad)? && *g != bad {
                bail!("good commit {} is not an ancestor of the bad commit {}", crate::short(g), crate::short(&bad));
            }
        }
        let candidates: Vec<ObjectId> = self
            .git
            .rev_walk([bad])
            .with_hidden(good.iter().copied())
            .all()?
            .map(|i| i.map(|i| i.id))
            .collect::<Result<_, _>>()?;
        let testable: Vec<ObjectId> = candidates.iter().copied().filter(|c| *c != bad && !skip.contains(c)).collect();
        if testable.is_empty() {
            let rest: Vec<ObjectId> = candidates.iter().copied().filter(|c| *c != bad && skip.contains(c)).collect();
            return Ok(if rest.is_empty() { BisectStep::Found(bad) } else { BisectStep::OnlySkipped(rest) });
        }
        // Pick the commit that splits the candidates most evenly (its ancestors among them).
        let n = candidates.len();
        let mut best = (0usize, testable[testable.len() / 2]);
        if n <= 2000 {
            for c in &testable {
                let reach = count_between(self, *c, &good)?;
                let score = reach.min(n - reach);
                if score > best.0 {
                    best = (score, *c);
                }
            }
        }
        let commit = best.1;
        self.switch(SwitchTarget::Detach(commit), false)?;
        let steps = (usize::BITS - n.leading_zeros()).saturating_sub(1);
        Ok(BisectStep::Test { commit, remaining: n, steps })
    }

    /// End the bisection and return to where it started.
    pub fn bisect_reset(&self) -> Result<String> {
        let state = self.bisect_state()?.context("no bisection in progress")?;
        if state.original.starts_with("refs/") {
            self.switch(SwitchTarget::Branch(state.original.clone()), false)?;
        } else {
            self.switch(SwitchTarget::Detach(ObjectId::from_hex(state.original.as_bytes())?), false)?;
        }
        for (name, _) in self.all_refs()? {
            if name.starts_with("refs/bisect/") {
                self.delete_ref(&name, None)?;
            }
        }
        for f in [
            "BISECT_START",
            "BISECT_TERMS",
            "BISECT_NAMES",
            "BISECT_LOG",
            "BISECT_EXPECTED_REV",
            "BISECT_ANCESTORS_OK",
            "BISECT_HEAD",
        ] {
            let _ = std::fs::remove_file(self.bisect_file(f));
        }
        let _ = std::fs::remove_file(self.legacy_bisect_path());
        Ok(crate::refs::shorten(&state.original).to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::CommitRequest;
    use crate::worktree::Pathspec;

    fn repo_with_history(n: usize) -> (tempfile::TempDir, Repo, Vec<ObjectId>) {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repo::init(dir.path(), Some("main")).unwrap();
        let cfg = repo.config_path(crate::config::Scope::Local).unwrap();
        crate::config::set_in_file(&cfg, "user.name", "T").unwrap();
        crate::config::set_in_file(&cfg, "user.email", "t@e").unwrap();
        repo.reload().unwrap();
        let mut ids = Vec::new();
        for i in 0..n {
            std::fs::write(dir.path().join("f.txt"), format!("{i}\n")).unwrap();
            repo.add(&Pathspec::all(), false).unwrap();
            ids.push(repo.commit(&CommitRequest { message: format!("c{i}\n"), ..Default::default() }).unwrap());
        }
        (dir, repo, ids)
    }

    #[test]
    fn bisect_finds_the_first_bad_commit() {
        let (_d, repo, ids) = repo_with_history(16);
        let first_bad = 11;
        repo.bisect_start().unwrap();
        repo.bisect_mark("good", ids[0]).unwrap();
        let mut step = repo.bisect_mark("bad", ids[15]).unwrap();
        let mut rounds = 0;
        loop {
            match step {
                BisectStep::Test { commit, .. } => {
                    rounds += 1;
                    let i = ids.iter().position(|c| *c == commit).unwrap();
                    step = repo.bisect_mark(if i >= first_bad { "bad" } else { "good" }, commit).unwrap();
                }
                BisectStep::Found(c) => {
                    assert_eq!(c, ids[first_bad]);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(rounds <= 5, "binary search: {rounds} rounds");
        assert_eq!(repo.bisect_reset().unwrap(), "main");
        assert_eq!(repo.head_commit_id().unwrap(), Some(ids[15]));
    }
}
