//! Releases: notes from Conventional Commits, preflight checks before tagging, and verification
//! of a released tag (its signature, the signatures of the commits it ships, and attestations).

use anyhow::Result;
use gix::ObjectId;
use serde::Serialize;

use crate::Repo;

/// A commit, classified by its Conventional Commits prefix.
#[derive(Debug, Clone, Serialize)]
pub struct NoteEntry {
    pub id: String,
    pub kind: String,
    pub scope: Option<String>,
    pub description: String,
    pub breaking: bool,
}

/// Parse `type(scope)!: description`. Anything else is kind `other`.
pub fn classify(message: &str) -> (String, Option<String>, String, bool) {
    let subject = message.lines().next().unwrap_or("").trim();
    let breaking_footer =
        message.lines().any(|l| l.starts_with("BREAKING CHANGE:") || l.starts_with("BREAKING-CHANGE:"));
    if let Some((head, desc)) = subject.split_once(": ") {
        let bang = head.ends_with('!');
        let head = head.trim_end_matches('!');
        let (kind, scope) = match head.split_once('(') {
            Some((k, s)) => (k, Some(s.trim_end_matches(')').to_owned())),
            None => (head, None),
        };
        if !kind.is_empty() && kind.chars().all(|c| c.is_ascii_alphanumeric()) {
            return (kind.to_ascii_lowercase(), scope, desc.trim().to_owned(), bang || breaking_footer);
        }
    }
    ("other".into(), None, subject.to_owned(), breaking_footer)
}

const SECTIONS: &[(&str, &str)] = &[
    ("feat", "Features"),
    ("fix", "Bug fixes"),
    ("perf", "Performance"),
    ("security", "Security"),
    ("refactor", "Refactoring"),
    ("docs", "Documentation"),
    ("test", "Tests"),
    ("build", "Build"),
    ("ci", "CI"),
    ("chore", "Chores"),
];

/// One result of the release preflight.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    /// Blocking checks stop `release prepare`; the others are warnings.
    pub blocking: bool,
    pub detail: String,
}

/// The result of `release verify`.
#[derive(Debug, Clone, Serialize)]
pub struct ReleaseVerification {
    pub tag: String,
    pub commit: String,
    pub tag_signed: Option<bool>,
    pub commits: usize,
    pub good_signatures: usize,
    pub bad_signatures: Vec<String>,
    pub unsigned: usize,
    pub attestations: Vec<String>,
}

impl ReleaseVerification {
    pub fn ok(&self) -> bool {
        self.tag_signed != Some(false) && self.bad_signatures.is_empty()
    }
}

impl Repo {
    /// The most recent release tag (a tag that looks like a version, `v1.2.3` or `1.2`) reachable
    /// from `commit`, and its commit.
    pub fn previous_tag(&self, commit: ObjectId) -> Result<Option<(String, ObjectId)>> {
        let is_version = |name: &str| {
            let core = name.trim_start_matches(['v', 'V']);
            let first = core.split(['-', '+']).next().unwrap_or("");
            let parts: Vec<&str> = first.split('.').collect();
            parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        };
        let mut by_commit: std::collections::HashMap<ObjectId, String> = Default::default();
        for (name, id) in self.tags()? {
            if is_version(&name)
                && let Ok(c) = self.peel_to_commit(id)
            {
                by_commit
                    .entry(c)
                    .and_modify(|n| {
                        if name > *n {
                            *n = name.clone()
                        }
                    })
                    .or_insert(name);
            }
        }
        if by_commit.is_empty() {
            return Ok(None);
        }
        for info in self.git.rev_walk([commit]).all()? {
            let id = info?.id;
            if let Some(name) = by_commit.get(&id) {
                return Ok(Some((name.clone(), id)));
            }
        }
        Ok(None)
    }

    /// Commits in `tip` since `base` (all of history when `None`), classified.
    pub fn release_entries(&self, tip: ObjectId, base: Option<ObjectId>) -> Result<Vec<NoteEntry>> {
        let walk = self.git.rev_walk([tip]).with_hidden(base).all()?;
        let mut out = Vec::new();
        for info in walk {
            let c = self.commit_info(info?.id)?;
            if c.parents.len() > 1 {
                continue; // merge commits repeat what they merge
            }
            let (kind, scope, description, breaking) = classify(&c.message);
            out.push(NoteEntry { id: c.id.to_string(), kind, scope, description, breaking });
        }
        out.reverse();
        Ok(out)
    }

    /// Markdown release notes for `entries`.
    pub fn render_release_notes(title: &str, entries: &[NoteEntry]) -> String {
        let mut out = format!("## {title}\n");
        let line = |e: &NoteEntry| {
            let scope = e.scope.as_deref().map(|s| format!("**{s}:** ")).unwrap_or_default();
            format!("- {scope}{} ({})\n", e.description, &e.id[..7.min(e.id.len())])
        };
        let breaking: Vec<&NoteEntry> = entries.iter().filter(|e| e.breaking).collect();
        if !breaking.is_empty() {
            out.push_str("\n### ⚠ Breaking changes\n\n");
            for e in breaking {
                out.push_str(&line(e));
            }
        }
        for (kind, heading) in SECTIONS {
            let items: Vec<&NoteEntry> = entries.iter().filter(|e| e.kind == *kind).collect();
            if !items.is_empty() {
                out.push_str(&format!("\n### {heading}\n\n"));
                for e in items {
                    out.push_str(&line(e));
                }
            }
        }
        let other: Vec<&NoteEntry> = entries.iter().filter(|e| !SECTIONS.iter().any(|(k, _)| *k == e.kind)).collect();
        if !other.is_empty() {
            out.push_str("\n### Other changes\n\n");
            for e in other {
                out.push_str(&line(e));
            }
        }
        if entries.is_empty() {
            out.push_str("\nNo changes.\n");
        }
        out
    }

    /// Preflight checks before tagging `version` at HEAD.
    pub fn release_checks(&self, version: &str) -> Result<Vec<Check>> {
        let mut checks = Vec::new();
        let mut add = |name: &str, ok: bool, blocking: bool, detail: String| {
            checks.push(Check { name: name.into(), ok, blocking, detail });
        };
        let head = self.head_commit_id()?;
        add("has commits", head.is_some(), true, "HEAD must point to a commit".into());
        let Some(head) = head else { return Ok(checks) };

        let tag_ref = format!("refs/tags/{version}");
        let exists = self.ref_target(&tag_ref)?.is_some();
        add(
            "tag is new",
            !exists,
            true,
            if exists { format!("{version} already exists") } else { format!("{version} is free") },
        );

        let semver = version.trim_start_matches('v');
        let valid = semver.split(['-', '+']).next().is_some_and(|core| {
            let parts: Vec<&str> = core.split('.').collect();
            parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        });
        add(
            "semantic version",
            valid,
            false,
            format!("{version} {} MAJOR.MINOR.PATCH", if valid { "is" } else { "is not" }),
        );

        let status = self.status()?;
        let clean = status.worktree.is_clean() && status.staged.is_empty();
        add(
            "clean working tree",
            clean,
            true,
            if clean { "nothing uncommitted".into() } else { "commit or stash your changes first".into() },
        );

        let branch = self.head_state()?.branch_short().map(str::to_owned);
        let default = self.default_branch()?;
        let on_default = branch.is_some() && branch == default;
        add(
            "on the default branch",
            on_default,
            false,
            format!(
                "on {}{}",
                branch.as_deref().unwrap_or("a detached HEAD"),
                default.map(|d| format!(", default is {d}")).unwrap_or_default()
            ),
        );

        if let Some(b) = &branch
            && let Some(upstream) = self.config_get(&format!("branch.{b}.merge"))
        {
            let remote = self.config_get(&format!("branch.{b}.remote")).unwrap_or_else(|| "origin".into());
            let tracking = format!("refs/remotes/{remote}/{}", upstream.trim_start_matches("refs/heads/"));
            if let Some(up) = self.ref_target(&tracking)? {
                let (ahead, behind) = self.ahead_behind(head, up)?;
                add(
                    "pushed",
                    ahead == 0 && behind == 0,
                    false,
                    format!("{ahead} ahead, {behind} behind {remote}/{}", upstream.trim_start_matches("refs/heads/")),
                );
            }
        }

        let since = self.previous_tag(head)?;
        let n = self.release_entries(head, since.as_ref().map(|(_, id)| *id))?.len();
        add(
            "changes since the last release",
            n > 0,
            true,
            match &since {
                Some((t, _)) => format!("{n} commit(s) since {t}"),
                None => format!("{n} commit(s); first release"),
            },
        );

        let sign = self.config_bool("tag.gpgSign").unwrap_or(false) || self.config_get("user.signingKey").is_some();
        add(
            "tag will be signed",
            sign,
            false,
            if sign {
                "signing is configured".into()
            } else {
                "set user.signingKey (and tag.gpgSign) to sign releases".into()
            },
        );

        let tested = self.attestations(head)?.iter().any(|a| a.predicate_type().contains("test"));
        add(
            "tests attested",
            tested,
            false,
            if tested {
                "HEAD has a test attestation".into()
            } else {
                "record one with `veyra attest run -- <test command>`".into()
            },
        );
        Ok(checks)
    }

    /// Verify a released tag and the history it ships.
    pub fn release_verify(&self, tag: &str) -> Result<ReleaseVerification> {
        let tag_id =
            self.ref_target(&format!("refs/tags/{tag}"))?.ok_or_else(|| anyhow::anyhow!("no tag named '{tag}'"))?;
        let commit = self.peel_to_commit(tag_id)?;
        let annotated = self.git.find_object(tag_id)?.kind == gix::object::Kind::Tag;
        let tag_signed = if annotated { self.verify_tag(tag_id)?.outcome.map(|o| o.valid) } else { None };
        let parent = self.commit_info(commit)?.parents.first().copied();
        let base = match parent {
            Some(p) => self.previous_tag(p)?.map(|(_, id)| id),
            None => None,
        };
        let walk = self.git.rev_walk([commit]).with_hidden(base).all()?;
        let (mut commits, mut good, mut unsigned) = (0, 0, 0);
        let mut bad = Vec::new();
        for info in walk {
            let id = info?.id;
            commits += 1;
            match self.verify_commit(id)?.outcome {
                Some(o) if o.valid => good += 1,
                Some(_) => bad.push(crate::short(&id)),
                None => unsigned += 1,
            }
        }
        let attestations = self.attestations(commit)?.iter().map(|a| a.predicate_type().to_owned()).collect();
        Ok(ReleaseVerification {
            tag: tag.to_owned(),
            commit: commit.to_string(),
            tag_signed,
            commits,
            good_signatures: good,
            bad_signatures: bad,
            unsigned,
            attestations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_commits() {
        assert_eq!(classify("feat(api): add x"), ("feat".into(), Some("api".into()), "add x".into(), false));
        assert!(classify("fix!: drop y").3);
        assert!(classify("refactor: z\n\nBREAKING CHANGE: gone").3);
        assert_eq!(classify("Update README").0, "other");
        let entries = vec![
            NoteEntry {
                id: "abcdef12".into(),
                kind: "feat".into(),
                scope: None,
                description: "new".into(),
                breaking: false,
            },
            NoteEntry {
                id: "12345678".into(),
                kind: "other".into(),
                scope: None,
                description: "misc".into(),
                breaking: false,
            },
        ];
        let notes = Repo::render_release_notes("v1.0.0", &entries);
        assert!(notes.contains("### Features\n\n- new (abcdef1)"), "{notes}");
        assert!(notes.contains("### Other changes"), "{notes}");
    }
}
