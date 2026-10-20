//! History views: `blame`, `log --graph`, and `clean`.

use std::collections::{BinaryHeap, HashMap, HashSet};

use anyhow::{Result, anyhow};
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};

use crate::Repo;
use crate::commit::CommitInfo;

/// One line of `blame` output.
#[derive(Debug, Clone)]
pub struct BlameLine {
    /// 1-based line number in the blamed file.
    pub line: usize,
    pub commit: ObjectId,
    pub author: String,
    pub time: i64,
    pub offset: i32,
    /// The path in the source commit, when it differs (the file was renamed since).
    pub source_path: Option<String>,
    pub text: String,
}

/// One row of a rendered history graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphRow {
    /// A commit line: graph prefix and the index of the commit in the input.
    Commit { prefix: String, index: usize },
    /// A connector line between commits (`|\`, `|/`).
    Edge(String),
}

impl Repo {
    /// Who last changed each line of `path` as of `rev`.
    pub fn blame(&self, path: &BStr, rev: ObjectId) -> Result<Vec<BlameLine>> {
        let outcome =
            self.git.blame_file(path, rev, Default::default()).map_err(|e| anyhow!("could not blame {path}: {e}"))?;
        let lines: Vec<&[u8]> = outcome.blob.lines_with_terminator().collect();
        let mut infos: HashMap<ObjectId, CommitInfo> = HashMap::new();
        let mut out = Vec::with_capacity(lines.len());
        for entry in &outcome.entries {
            let info = match infos.entry(entry.commit_id) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(self.commit_info(entry.commit_id)?),
            };
            let start = entry.start_in_blamed_file as usize;
            for i in start..start + entry.len.get() as usize {
                let text = lines
                    .get(i)
                    .map(|l| l.trim_end_with(|c| c == '\n' || c == '\r').to_str_lossy().into_owned())
                    .unwrap_or_default();
                out.push(BlameLine {
                    line: i + 1,
                    commit: entry.commit_id,
                    author: info.author_name.clone(),
                    time: info.author_time,
                    offset: info.author_offset,
                    source_path: entry.source_file_name.as_ref().map(|p| p.to_string()),
                    text,
                });
            }
        }
        out.sort_by_key(|l| l.line);
        Ok(out)
    }

    /// Commits reachable from `tips` in topological order (children before parents, newer first
    /// among siblings), like `git log --graph`. At most `limit` commits are considered.
    pub fn topo_log(&self, tips: &[ObjectId], limit: Option<usize>) -> Result<Vec<CommitInfo>> {
        let commits = self.log(tips, limit)?;
        let ids: HashSet<ObjectId> = commits.iter().map(|c| c.id).collect();
        let mut children: HashMap<ObjectId, usize> = HashMap::new();
        for c in &commits {
            for p in &c.parents {
                if ids.contains(p) {
                    *children.entry(*p).or_default() += 1;
                }
            }
        }
        let by_id: HashMap<ObjectId, &CommitInfo> = commits.iter().map(|c| (c.id, c)).collect();
        let mut ready: BinaryHeap<(i64, ObjectId)> =
            commits.iter().filter(|c| !children.contains_key(&c.id)).map(|c| (c.committer_time, c.id)).collect();
        let mut out = Vec::with_capacity(commits.len());
        while let Some((_, id)) = ready.pop() {
            let c = by_id[&id];
            out.push(c.clone());
            for p in &c.parents {
                if let Some(n) = children.get_mut(p) {
                    *n -= 1;
                    if *n == 0 {
                        ready.push((by_id[p].committer_time, *p));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Remove untracked (non-ignored) files. Without `dirs`, files inside wholly untracked
    /// directories are left alone, like `git clean` without `-d`. Returns the removed paths.
    pub fn clean(&self, paths: &crate::worktree::Pathspec, dirs: bool, dry_run: bool) -> Result<Vec<BString>> {
        let changes = self.worktree_changes()?;
        let index = self.flat_index()?;
        let tracked_dirs: HashSet<BString> = index
            .keys()
            .flat_map(|p| {
                let bytes = p.as_bytes();
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b == b'/')
                    .map(|(i, _)| BString::from(&bytes[..i]))
                    .collect::<Vec<_>>()
            })
            .collect();
        let doomed: Vec<BString> = changes
            .untracked
            .iter()
            .filter(|p| paths.matches(p.as_ref()))
            .filter(|p| {
                dirs || match p.rfind_byte(b'/') {
                    None => true,
                    Some(i) => tracked_dirs.contains(&BString::from(&p[..i])),
                }
            })
            .cloned()
            .collect();
        if !dry_run && !doomed.is_empty() {
            let deletions: Vec<_> = doomed.iter().map(|p| (p.clone(), None)).collect();
            self.write_worktree(&deletions)?;
        }
        Ok(doomed)
    }
}

/// Render `git log --graph`-style lanes for commits already in topological order.
pub fn render_graph(commits: &[CommitInfo]) -> Vec<GraphRow> {
    let mut rows = Vec::new();
    let mut lanes: Vec<Option<ObjectId>> = Vec::new();
    let cell = |c: &str| format!("{c} ");
    for (index, commit) in commits.iter().enumerate() {
        let col = match lanes.iter().position(|l| *l == Some(commit.id)) {
            Some(i) => i,
            None => match lanes.iter().position(Option::is_none) {
                Some(free) => {
                    lanes[free] = Some(commit.id);
                    free
                }
                None => {
                    lanes.push(Some(commit.id));
                    lanes.len() - 1
                }
            },
        };
        let prefix: String = lanes
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if i == col {
                    cell("*")
                } else if l.is_some() {
                    cell("|")
                } else {
                    cell(" ")
                }
            })
            .collect();
        rows.push(GraphRow::Commit { prefix, index });

        // Continue the lane with the first parent; extra parents open new lanes to the right.
        lanes[col] = commit.parents.first().copied();
        let mut opened = 0;
        for p in commit.parents.iter().skip(1) {
            if !lanes.contains(&Some(*p)) {
                lanes.insert(col + 1 + opened, Some(*p));
                opened += 1;
            }
        }
        if commit.parents.len() > 1 {
            let mut edge = String::new();
            for (i, l) in lanes.iter().enumerate() {
                if i == col {
                    edge.push_str(if opened > 0 { "|\\" } else { "|" });
                } else if i > col && i <= col + opened {
                    edge.push(if i == col + opened { ' ' } else { '\\' });
                    edge.push(' ');
                } else {
                    edge.push_str(if l.is_some() { "| " } else { "  " });
                }
            }
            rows.push(GraphRow::Edge(edge.trim_end().to_owned()));
        }

        // Two lanes waiting for the same commit merge back together.
        loop {
            let mut merged = None;
            'outer: for (i, a) in lanes.iter().enumerate() {
                if a.is_none() {
                    continue;
                }
                for (j, b) in lanes.iter().enumerate().skip(i + 1) {
                    if a == b {
                        merged = Some((i, j));
                        break 'outer;
                    }
                }
            }
            let Some((i, j)) = merged else { break };
            let mut edge = String::new();
            for (k, l) in lanes.iter().enumerate() {
                if k == j {
                    edge.push_str("/ ");
                } else if k == i || l.is_some() {
                    edge.push_str(if k + 1 == j { "|" } else { "| " });
                } else {
                    edge.push_str("  ");
                }
            }
            rows.push(GraphRow::Edge(edge.trim_end().to_owned()));
            lanes.remove(j);
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ObjectId {
        let mut b = [0u8; 20];
        b[19] = n;
        ObjectId::from_bytes_or_panic(&b)
    }

    fn commit(n: u8, parents: &[u8]) -> CommitInfo {
        CommitInfo {
            id: id(n),
            tree: id(0),
            parents: parents.iter().map(|p| id(*p)).collect(),
            author_name: String::new(),
            author_email: String::new(),
            author_time: 0,
            author_offset: 0,
            committer_name: String::new(),
            committer_email: String::new(),
            committer_time: 0,
            message: String::new(),
        }
    }

    fn render(commits: &[CommitInfo]) -> String {
        render_graph(commits)
            .into_iter()
            .map(|r| match r {
                GraphRow::Commit { prefix, index } => format!("{}{}", prefix, index),
                GraphRow::Edge(e) => e,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn linear_history_is_a_single_lane() {
        let commits = vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])];
        assert_eq!(render(&commits), "* 0\n* 1\n* 2");
    }

    #[test]
    fn a_merge_opens_and_closes_a_lane() {
        // 4 = merge(3, 2); 3 -> 1; 2 -> 1
        let commits = vec![commit(4, &[3, 2]), commit(2, &[1]), commit(3, &[1]), commit(1, &[])];
        assert_eq!(render(&commits), "* 0\n|\\\n| * 1\n* | 2\n|/\n* 3");
    }
}
