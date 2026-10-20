//! Creating and reading commits.

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;

use crate::{HeadState, Repo, flat};

/// Everything needed to create a commit from the current index.
#[derive(Debug, Clone, Default)]
pub struct CommitRequest {
    pub message: String,
    /// Replace the current HEAD commit instead of adding a new one.
    pub amend: bool,
    /// Allow a commit whose tree equals its parent's.
    pub allow_empty: bool,
    /// Trailers appended to the message, e.g. `("Assisted-by", "Claude")`.
    pub trailers: Vec<(String, String)>,
    /// Use this author instead of the configured identity (cherry-pick and rebase keep the original author).
    pub author: Option<gix::actor::Signature>,
}

/// A decoded commit, for display.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub author_offset: i32,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_time: i64,
    pub message: String,
}

impl CommitInfo {
    /// The first line of the message.
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim()
    }
}

/// Normalise a message like `git commit --cleanup=whitespace` (or `strip` when `strip_comments`).
pub fn clean_message(message: &str, strip_comments: bool) -> String {
    let mut lines: Vec<&str> =
        message.lines().filter(|l| !(strip_comments && l.starts_with('#'))).map(str::trim_end).collect();
    // Collapse runs of blank lines and trim leading/trailing blank lines.
    let mut out: Vec<&str> = Vec::new();
    for line in lines.drain(..) {
        if line.is_empty() && out.last().is_none_or(|l: &&str| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        return String::new();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Append `trailers` to `message`, joining an existing trailer block if there is one.
pub fn add_trailers(message: &str, trailers: &[(String, String)]) -> String {
    if trailers.is_empty() {
        return message.to_owned();
    }
    let body = message.trim_end_matches('\n');
    let last_paragraph = body.rsplit("\n\n").next().unwrap_or("");
    let has_trailer_block =
        body.contains("\n\n") && !last_paragraph.is_empty() && last_paragraph.lines().all(is_trailer_line);
    let mut out = String::from(body);
    out.push_str(if has_trailer_block { "\n" } else { "\n\n" });
    for (i, (key, value)) in trailers.iter().enumerate() {
        let line = format!("{key}: {value}");
        if body.lines().any(|l| l == line) {
            continue; // don't duplicate
        }
        if i > 0 && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&line);
        out.push('\n');
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn is_trailer_line(line: &str) -> bool {
    match line.split_once(": ") {
        Some((key, _)) => !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        None => false,
    }
}

/// Parse `Key: value` trailers from the last paragraph of a message.
pub fn parse_trailers(message: &str) -> Vec<(String, String)> {
    let body = message.trim_end();
    if !body.contains("\n\n") {
        return Vec::new();
    }
    let last = body.rsplit("\n\n").next().unwrap_or("");
    if !last.lines().all(is_trailer_line) {
        return Vec::new();
    }
    last.lines().filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_owned(), v.trim().to_owned()))).collect()
}

impl Repo {
    /// Create a commit from the index and advance HEAD (or the branch it points to).
    pub fn commit(&self, request: &CommitRequest) -> Result<ObjectId> {
        self.require_identity()?;
        let index = self.load_index()?;
        if index.entries().iter().any(|e| e.stage_raw() != 0) {
            bail!(
                "cannot commit: there are unresolved conflicts\n  hint: fix them, `veyra add` the files, then commit"
            );
        }
        let flat = flat::flat_from_index_state(&index);
        let tree = self.write_flat(&flat)?;

        let head = self.head_state()?;
        let head_commit = head.commit();
        let (parents, author) = if request.amend {
            let old = head_commit.ok_or_else(|| anyhow!("nothing to amend: the current branch has no commits yet"))?;
            let old = self.git.find_commit(old)?;
            let parents: Vec<ObjectId> = old.parent_ids().map(|id| id.detach()).collect();
            let author: gix::actor::Signature = old.author()?.to_owned()?;
            (parents, Some(author))
        } else {
            (head_commit.into_iter().collect(), None)
        };

        if !request.allow_empty && !request.amend {
            let parent_tree = match parents.first() {
                Some(p) => self.commit_tree_id(*p)?,
                None => self.empty_tree_id(),
            };
            if parent_tree == tree {
                bail!("nothing to commit (use `veyra add` to stage changes, or --allow-empty)");
            }
        }

        let message = add_trailers(&request.message, &request.trailers);
        if message.trim().is_empty() {
            bail!("aborting commit due to empty commit message");
        }

        let committer: gix::actor::Signature =
            self.git.committer().ok_or_else(|| anyhow!("committer identity is not configured"))??.to_owned()?;
        let author = match request.author.clone().or(author) {
            Some(a) => a,
            None => self.git.author().ok_or_else(|| anyhow!("author identity is not configured"))??.to_owned()?,
        };

        let commit = gix::objs::Commit {
            message: message.as_str().into(),
            tree,
            author,
            committer,
            encoding: None,
            parents: parents.iter().copied().collect(),
            extra_headers: Default::default(),
        };
        let id = self.git.write_object(&commit)?.detach();

        let summary = message.lines().next().unwrap_or_default();
        let reflog = match (request.amend, head_commit) {
            (true, _) => format!("commit (amend): {summary}"),
            (false, None) => format!("commit (initial): {summary}"),
            (false, Some(_)) => format!("commit: {summary}"),
        };
        self.update_head_target(id, head_commit, &reflog)?;
        Ok(id)
    }

    /// Decode a commit for display.
    pub fn commit_info(&self, id: ObjectId) -> Result<CommitInfo> {
        let commit = self.git.find_commit(id).with_context(|| format!("{id} is not a commit"))?;
        let decoded = commit.decode()?;
        let author = decoded.author()?;
        let committer = decoded.committer()?;
        let author_time = author.time()?;
        let committer_time = committer.time()?;
        Ok(CommitInfo {
            id,
            tree: decoded.tree(),
            parents: decoded.parents().collect(),
            author_name: author.name.to_str_lossy().into_owned(),
            author_email: author.email.to_str_lossy().into_owned(),
            author_time: author_time.seconds,
            author_offset: author_time.offset,
            committer_name: committer.name.to_str_lossy().into_owned(),
            committer_email: committer.email.to_str_lossy().into_owned(),
            committer_time: committer_time.seconds,
            message: decoded.message.to_str_lossy().into_owned(),
        })
    }

    /// Walk history from `tips` (newest first by commit time). `limit` caps the count.
    pub fn log(&self, tips: &[ObjectId], limit: Option<usize>) -> Result<Vec<CommitInfo>> {
        if tips.is_empty() {
            return Ok(Vec::new());
        }
        let walk = self
            .git
            .rev_walk(tips.iter().copied())
            .sorting(gix::revision::walk::Sorting::ByCommitTime(Default::default()))
            .all()?;
        let mut out = Vec::new();
        for info in walk {
            if limit.is_some_and(|l| out.len() >= l) {
                break;
            }
            let info = info.map_err(|e| anyhow!("history traversal failed: {e}"))?;
            out.push(self.commit_info(info.id)?);
        }
        Ok(out)
    }

    /// Warn-worthy facts about the commit environment Veyra doesn't fully support yet.
    pub fn commit_caveats(&self) -> Vec<String> {
        let mut out = Vec::new();
        let config = self.git.config_snapshot();
        if config.boolean("commit.gpgsign").unwrap_or(false) {
            out.push("commit.gpgsign is enabled, but Veyra cannot sign commits yet; this commit is unsigned".into());
        }
        out
    }

    /// The current branch name for messages, e.g. `main` or `HEAD (detached)`.
    pub fn head_label(&self) -> Result<String> {
        Ok(match self.head_state()? {
            HeadState::Branch { name, .. } => crate::refs::shorten(&name.to_str_lossy()).to_owned(),
            HeadState::Detached(id) => format!("HEAD (detached at {})", crate::short(&id)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_collapses_blank_lines_and_strips_comments() {
        assert_eq!(clean_message("\n\nsubject  \n\n\n\nbody\n\n", false), "subject\n\nbody\n");
        assert_eq!(clean_message("subject\n# comment\n", true), "subject\n");
        assert_eq!(clean_message("# only comments\n", true), "");
    }

    #[test]
    fn trailers_start_a_new_block_or_join_an_existing_one() {
        let t = vec![("Assisted-by".to_owned(), "Claude".to_owned())];
        assert_eq!(add_trailers("feat: x\n", &t), "feat: x\n\nAssisted-by: Claude\n");
        assert_eq!(
            add_trailers("feat: x\n\nSigned-off-by: A <a@b>\n", &t),
            "feat: x\n\nSigned-off-by: A <a@b>\nAssisted-by: Claude\n"
        );
        // A prose paragraph is not a trailer block.
        assert_eq!(
            add_trailers("feat: x\n\nSome body text.\n", &t),
            "feat: x\n\nSome body text.\n\nAssisted-by: Claude\n"
        );
    }

    #[test]
    fn trailers_round_trip() {
        let msg = "feat: x\n\nbody\n\nIntent: int-1\nAssisted-by: Claude\n";
        assert_eq!(
            parse_trailers(msg),
            vec![("Intent".into(), "int-1".into()), ("Assisted-by".into(), "Claude".into())]
        );
        assert!(parse_trailers("subject only\n").is_empty());
    }
}
