//! Patches: `format-patch` (mbox), `apply` (unified/Git diffs) and `am` (apply mailed patches).
//!
//! The diff format is Git's (`diff --git`, `new file mode`, `@@` hunks, `\ No newline at end of
//! file`); plain `diff -u` output works too. Hunks are applied at their stated position, or at the
//! nearest position where their context matches exactly — never with fuzzy context.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::Repo;
use crate::commit::CommitRequest;
use crate::diff::NewSide;
use crate::flat::{self, FlatEntry, FlatTree};

/// One hunk: lines tagged ' ' (context), '-' (removed) or '+' (added), with their line endings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub lines: Vec<(char, String)>,
}

/// The changes to one file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilePatch {
    /// `None` for a new file.
    pub old_path: Option<String>,
    /// `None` for a deleted file.
    pub new_path: Option<String>,
    pub executable: Option<bool>,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FilePatch {
    pub fn path(&self) -> &str {
        self.new_path.as_deref().or(self.old_path.as_deref()).unwrap_or("")
    }

    fn reversed(&self) -> FilePatch {
        FilePatch {
            old_path: self.new_path.clone(),
            new_path: self.old_path.clone(),
            executable: self.executable,
            binary: self.binary,
            hunks: self
                .hunks
                .iter()
                .map(|h| Hunk {
                    old_start: h.old_start,
                    lines: h
                        .lines
                        .iter()
                        .map(|(t, l)| {
                            let t = match t {
                                '+' => '-',
                                '-' => '+',
                                c => *c,
                            };
                            (t, l.clone())
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

fn strip_prefix(path: &str) -> Option<String> {
    let path = path.trim_end_matches(['\r', '\n']).split('\t').next().unwrap_or("");
    if path == "/dev/null" {
        return None;
    }
    let path = path.trim_matches('"');
    Some(path.strip_prefix("a/").or_else(|| path.strip_prefix("b/")).unwrap_or(path).to_owned())
}

fn parse_range(s: &str) -> Option<usize> {
    s.split(',').next()?.parse().ok()
}

/// Parse a unified diff into per-file patches.
pub fn parse_patch(text: &str) -> Result<Vec<FilePatch>> {
    let mut out: Vec<FilePatch> = Vec::new();
    let mut current: Option<FilePatch> = None;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0;
    let finish = |current: &mut Option<FilePatch>, out: &mut Vec<FilePatch>| {
        if let Some(p) = current.take() {
            out.push(p);
        }
    };
    while i < lines.len() {
        let line = lines[i];
        let bare = line.trim_end_matches(['\r', '\n']);
        if let Some(rest) = bare.strip_prefix("diff --git ") {
            finish(&mut current, &mut out);
            // `a/x b/x`: split in the middle when both halves are equal (paths may contain spaces).
            let (a, b) = match rest.find(" b/") {
                Some(pos) => (&rest[..pos], &rest[pos + 1..]),
                None => (rest, rest),
            };
            current = Some(FilePatch { old_path: strip_prefix(a), new_path: strip_prefix(b), ..Default::default() });
        } else if bare.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ ")) {
            let old = strip_prefix(&bare[4..]);
            let new = strip_prefix(lines[i + 1].trim_end_matches(['\r', '\n']).get(4..).unwrap_or(""));
            match &mut current {
                Some(p) if p.hunks.is_empty() => {
                    p.old_path = old;
                    p.new_path = new;
                }
                _ => {
                    finish(&mut current, &mut out);
                    current = Some(FilePatch { old_path: old, new_path: new, ..Default::default() });
                }
            }
            i += 2;
            continue;
        } else if let Some(p) = current.as_mut() {
            if bare.starts_with("new file mode") {
                p.old_path = None;
                p.executable = Some(bare.ends_with("755"));
            } else if bare.starts_with("deleted file mode") {
                p.new_path = None;
            } else if let Some(m) = bare.strip_prefix("new mode ") {
                p.executable = Some(m.ends_with("755"));
            } else if bare.starts_with("Binary files ") || bare == "GIT binary patch" {
                p.binary = true;
            } else if let Some(h) = bare.strip_prefix("@@ -") {
                let old = h.split_whitespace().next().unwrap_or("");
                let old_start = parse_range(old).with_context(|| format!("bad hunk header: {bare}"))?;
                let mut hunk = Hunk { old_start, lines: Vec::new() };
                i += 1;
                while i < lines.len() {
                    let l = lines[i];
                    match l.chars().next() {
                        Some(t @ (' ' | '-' | '+')) => hunk.lines.push((t, l[1..].to_owned())),
                        Some('\\') => {
                            // "\ No newline at end of file": the previous line has no line ending.
                            if let Some((_, prev)) = hunk.lines.last_mut() {
                                let trimmed = prev.trim_end_matches('\n').trim_end_matches('\r').to_owned();
                                *prev = trimmed;
                            }
                        }
                        // Some tools emit empty context lines without the leading space.
                        Some('\n') | Some('\r') => hunk.lines.push((' ', l.to_owned())),
                        _ => break,
                    }
                    i += 1;
                }
                p.hunks.push(hunk);
                continue;
            }
        }
        i += 1;
    }
    finish(&mut current, &mut out);
    if out.is_empty() {
        bail!("no patch found in the input");
    }
    Ok(out)
}

/// Apply `hunks` to `content`. Each hunk goes at its stated line, or the nearest place its
/// context matches exactly.
pub fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String> {
    let mut lines: Vec<String> = content.split_inclusive('\n').map(str::to_owned).collect();
    let mut shift: isize = 0;
    for (n, h) in hunks.iter().enumerate() {
        let old: Vec<&String> = h.lines.iter().filter(|(t, _)| *t != '+').map(|(_, l)| l).collect();
        let new: Vec<String> = h.lines.iter().filter(|(t, _)| *t != '-').map(|(_, l)| l.clone()).collect();
        let wanted = (h.old_start as isize - 1 + shift).max(0) as usize;
        let fits =
            |at: usize| at + old.len() <= lines.len() && old.iter().enumerate().all(|(k, l)| lines[at + k] == **l);
        let at = if old.is_empty() {
            Some(wanted.min(lines.len()))
        } else {
            (0..=lines.len())
                .flat_map(|d| [wanted.checked_add(d), wanted.checked_sub(d)])
                .flatten()
                .find(|&at| fits(at))
        };
        let at = at.ok_or_else(|| anyhow!("hunk {} (at line {}) does not apply", n + 1, h.old_start))?;
        lines.splice(at..at + old.len(), new.iter().cloned());
        shift += new.len() as isize - old.len() as isize + (at as isize - wanted as isize);
    }
    Ok(lines.concat())
}

/// Where `apply` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyTo {
    /// The working tree only (`git apply`).
    Worktree,
    /// The index only (`git apply --cached`).
    Index,
    /// Both (`git apply --index`).
    Both,
}

/// A parsed mailed patch (`format-patch` output).
#[derive(Debug, Clone)]
pub struct MailPatch {
    pub author_name: String,
    pub author_email: String,
    /// Seconds and UTC offset.
    pub date: Option<(i64, i32)>,
    pub message: String,
    pub patch: String,
}

fn decode_header(value: &str) -> String {
    // Minimal RFC 2047 (`=?UTF-8?q?…?=` and `=?UTF-8?b?…?=` are what Git writes).
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let parts: Vec<&str> = after.splitn(3, '?').collect();
        let Some(end) = parts.get(2).and_then(|p| p.find("?=")) else {
            out.push_str(&rest[start..]);
            return out;
        };
        let (encoding, data) = (parts[1].to_ascii_lowercase(), &parts[2][..end]);
        let bytes = if encoding == "q" {
            let mut b = Vec::new();
            let raw = data.as_bytes();
            let mut k = 0;
            while k < raw.len() {
                match raw[k] {
                    b'_' => b.push(b' '),
                    b'=' if k + 2 < raw.len() => {
                        if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&raw[k + 1..k + 3]).unwrap_or(""), 16) {
                            b.push(v);
                            k += 2;
                        }
                    }
                    c => b.push(c),
                }
                k += 1;
            }
            b
        } else {
            data.as_bytes().to_vec()
        };
        out.push_str(&String::from_utf8_lossy(&bytes));
        rest = &parts[2][end + 2..];
    }
    out.push_str(rest);
    out
}

/// Split an mbox (one or more `format-patch` messages) into patches.
pub fn parse_mbox(text: &str) -> Result<Vec<MailPatch>> {
    let mut messages: Vec<Vec<&str>> = Vec::new();
    for line in text.split_inclusive('\n') {
        let is_separator =
            line.starts_with("From ") && line.len() > 46 && line[5..45].bytes().all(|b| b.is_ascii_hexdigit());
        if is_separator || messages.is_empty() {
            messages.push(Vec::new());
            if is_separator {
                continue;
            }
        }
        messages.last_mut().expect("pushed").push(line);
    }
    let mut out = Vec::new();
    for lines in messages {
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        let mut i = 0;
        let mut last_key = String::new();
        while i < lines.len() {
            let l = lines[i].trim_end_matches(['\r', '\n']);
            i += 1;
            if l.is_empty() {
                break;
            }
            if (l.starts_with(' ') || l.starts_with('\t')) && !last_key.is_empty() {
                let v = headers.entry(last_key.clone()).or_default();
                v.push(' ');
                v.push_str(l.trim());
            } else if let Some((k, v)) = l.split_once(':') {
                last_key = k.trim().to_ascii_lowercase();
                headers.insert(last_key.clone(), v.trim().to_owned());
            }
        }
        let Some(subject) = headers.get("subject") else { continue };
        let mut subject = decode_header(subject);
        if subject.starts_with('[')
            && let Some(end) = subject.find(']')
        {
            subject = subject[end + 1..].trim().to_owned();
        }
        let from = decode_header(headers.get("from").map(String::as_str).unwrap_or(""));
        let (name, email) = match (from.find('<'), from.find('>')) {
            (Some(a), Some(b)) if a < b => (from[..a].trim().trim_matches('"').to_owned(), from[a + 1..b].to_owned()),
            _ => (from.clone(), from.clone()),
        };
        let date = headers
            .get("date")
            .and_then(|d| jiff::fmt::rfc2822::parse(d).ok())
            .map(|z| (z.timestamp().as_second(), z.offset().seconds()));
        // Body up to the `---` line; the patch is everything from the first diff on.
        let rest: Vec<&str> = lines[i..].to_vec();
        let sep = rest.iter().position(|l| l.trim_end() == "---");
        let diff_start = rest.iter().position(|l| l.starts_with("diff --git ") || l.starts_with("--- "));
        let body_end = sep.or(diff_start).unwrap_or(rest.len());
        let body: String = rest[..body_end].concat();
        let mut patch: String = diff_start.map(|d| rest[d..].concat()).unwrap_or_default();
        if let Some(sig) = patch.rfind("\n-- \n") {
            patch.truncate(sig + 1);
        }
        let mut message = subject;
        if !body.trim().is_empty() {
            message.push_str("\n\n");
            message.push_str(body.trim_end());
        }
        message.push('\n');
        out.push(MailPatch { author_name: name, author_email: email, date, message, patch });
    }
    if out.is_empty() {
        bail!("no patches found in the mailbox");
    }
    Ok(out)
}

fn slug(subject: &str) -> String {
    let mut s: String =
        subject.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' { c } else { '-' }).collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches(['-', '.']).chars().take(52).collect::<String>();
    s.trim_end_matches(['-', '.']).to_owned()
}

impl Repo {
    /// `format-patch`: one mbox message per commit (oldest first). Returns `(file name, text)`.
    pub fn format_patches(&self, commits: &[ObjectId]) -> Result<Vec<(String, String)>> {
        let total = commits.len();
        let mut out = Vec::new();
        for (i, id) in commits.iter().enumerate() {
            let c = self.commit_info(*id)?;
            let parent = match c.parents.first() {
                Some(p) => self.flat_from_commit(*p)?,
                None => FlatTree::new(),
            };
            let changes = flat::diff(&parent, &self.flat_from_tree(c.tree)?);
            let diffs = self.diff_changes(&changes, NewSide::Objects, 3)?;
            let date = jiff::Timestamp::from_second(c.author_time)
                .ok()
                .zip(jiff::tz::Offset::from_seconds(c.author_offset).ok())
                .map(|(t, o)| t.to_zoned(o.to_time_zone()).strftime("%a, %-d %b %Y %H:%M:%S %z").to_string())
                .unwrap_or_default();
            let subject = c.summary().to_owned();
            let numbering = if total > 1 { format!("[PATCH {}/{total}]", i + 1) } else { "[PATCH]".into() };
            let body = c.message.trim_end().split_once('\n').map(|(_, b)| b.trim_start_matches('\n')).unwrap_or("");
            let mut text = format!(
                "From {id} Mon Sep 17 00:00:00 2001\nFrom: {} <{}>\nDate: {date}\nSubject: {numbering} {subject}\n\n",
                c.author_name, c.author_email
            );
            if !body.is_empty() {
                text.push_str(body);
                text.push('\n');
            }
            text.push_str("---\n");
            for d in &diffs {
                text.push_str(&format!(" {} | {}\n", d.path, d.added + d.removed));
            }
            text.push_str(&format!(
                " {} changed\n\n",
                if diffs.len() == 1 { "1 file".into() } else { format!("{} files", diffs.len()) }
            ));
            for d in &diffs {
                text.push_str(&crate::diff::render(d, None));
            }
            text.push_str(&format!("-- \nveyra {}\n\n", env!("CARGO_PKG_VERSION")));
            out.push((format!("{:04}-{}.patch", i + 1, slug(&subject)), text));
        }
        Ok(out)
    }

    /// Apply parsed patches. Everything is checked first; nothing is written unless every file
    /// applies. With `check`, nothing is written at all. Returns the touched paths.
    pub fn apply_patches(&self, patches: &[FilePatch], to: ApplyTo, reverse: bool, check: bool) -> Result<Vec<String>> {
        let patches: Vec<FilePatch> =
            if reverse { patches.iter().map(FilePatch::reversed).collect() } else { patches.to_vec() };
        let index = self.flat_index()?;
        let workdir = self.workdir()?.to_owned();
        let mut results: Vec<(FilePatch, Option<String>)> = Vec::new();
        for p in &patches {
            if p.binary {
                bail!("{}: binary patches are not supported", p.path());
            }
            let current = match &p.old_path {
                None => None,
                Some(old) => Some(match to {
                    ApplyTo::Index => {
                        let e = index
                            .get(&BString::from(old.as_str()))
                            .with_context(|| format!("{old}: not in the index"))?;
                        String::from_utf8(self.read_blob(e.id)?).with_context(|| format!("{old}: not text"))?
                    }
                    _ => std::fs::read_to_string(workdir.join(crate::repo::fs_path(old.as_bytes().as_bstr())?))
                        .with_context(|| format!("{old}: cannot read"))?,
                }),
            };
            if p.old_path.is_none() {
                let target = p.new_path.as_deref().unwrap_or_default();
                if index.contains_key(&BString::from(target)) || (to != ApplyTo::Index && workdir.join(target).exists())
                {
                    bail!("{target}: already exists");
                }
            }
            let new = match &p.new_path {
                None => None,
                Some(_) => {
                    Some(apply_hunks(current.as_deref().unwrap_or(""), &p.hunks).with_context(|| p.path().to_owned())?)
                }
            };
            results.push((p.clone(), new));
        }
        if check {
            return Ok(results.iter().map(|(p, _)| p.path().to_owned()).collect());
        }
        let mut new_index = index.clone();
        let mut worktree_writes = Vec::new();
        for (p, new) in &results {
            if let Some(old) = &p.old_path
                && p.new_path.as_deref() != Some(old.as_str())
            {
                new_index.remove(&BString::from(old.as_str()));
                worktree_writes.push((BString::from(old.as_str()), None));
            }
            if let (Some(path), Some(text)) = (&p.new_path, new) {
                let previous = index.get(&BString::from(path.as_str())).map(|e| e.kind);
                let kind = match (p.executable, previous) {
                    (Some(true), _) => EntryKind::BlobExecutable,
                    (Some(false), _) => EntryKind::Blob,
                    (None, Some(k)) => k,
                    (None, None) => EntryKind::Blob,
                };
                let entry = FlatEntry::new(kind, self.write_blob(text.as_bytes())?);
                new_index.insert(BString::from(path.as_str()), entry);
                worktree_writes.push((BString::from(path.as_str()), Some(entry)));
            }
        }
        if to != ApplyTo::Index {
            self.write_worktree(&worktree_writes)?;
        }
        if to != ApplyTo::Worktree {
            self.write_index_from_flat(&new_index, &Default::default())?;
        }
        Ok(results.iter().map(|(p, _)| p.path().to_owned()).collect())
    }

    /// `am`: apply one mailed patch to the index and worktree and commit it with its author.
    pub fn apply_mail(&self, mail: &MailPatch) -> Result<ObjectId> {
        if self.flat_index()? != self.flat_head()? {
            bail!("your index has staged changes; commit or unstage them first");
        }
        let patches = parse_patch(&mail.patch)?;
        self.apply_patches(&patches, ApplyTo::Both, false, false)?;
        let (seconds, offset) = mail.date.unwrap_or((crate::time::now_seconds(), 0));
        let author = gix::actor::Signature {
            name: mail.author_name.as_str().into(),
            email: mail.author_email.as_str().into(),
            time: gix::date::Time { seconds, offset },
        };
        self.commit(&CommitRequest { message: mail.message.clone(), author: Some(author), ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/a.txt b/a.txt\nindex 1111111..2222222 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..3333333\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n\\ No newline at end of file\n";

    #[test]
    fn parses_git_patches() {
        let p = parse_patch(PATCH).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].path(), "a.txt");
        assert_eq!(p[1].old_path, None);
        assert_eq!(p[1].hunks[0].lines, vec![('+', "hello".to_string())]);
    }

    #[test]
    fn applies_with_offsets_and_refuses_mismatches() {
        let p = parse_patch(PATCH).unwrap();
        assert_eq!(apply_hunks("one\ntwo\nthree\n", &p[0].hunks).unwrap(), "one\nTWO\nthree\n");
        // Two lines inserted above: the hunk still finds its context.
        assert_eq!(apply_hunks("x\ny\none\ntwo\nthree\n", &p[0].hunks).unwrap(), "x\ny\none\nTWO\nthree\n");
        assert!(apply_hunks("one\nchanged\nthree\n", &p[0].hunks).is_err());
        assert_eq!(apply_hunks("", &p[1].hunks).unwrap(), "hello");
        let reversed = p[0].reversed();
        assert_eq!(apply_hunks("one\nTWO\nthree\n", &reversed.hunks).unwrap(), "one\ntwo\nthree\n");
    }

    #[test]
    fn mbox_round_trip_fields() {
        let mbox = format!(
            "From 1234567890123456789012345678901234567890 Mon Sep 17 00:00:00 2001\nFrom: =?UTF-8?q?Jos=C3=A9?= <jose@example.com>\nDate: Tue, 20 Oct 2026 10:15:00 +0200\nSubject: [PATCH 1/2] fix: things\n\nBecause.\n---\n a.txt | 2 +-\n\n{PATCH}-- \nveyra 1.0.0\n"
        );
        let m = parse_mbox(&mbox).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].author_name, "José");
        assert_eq!(m[0].message, "fix: things\n\nBecause.\n");
        assert_eq!(m[0].date.unwrap().1, 7200);
        assert!(m[0].patch.starts_with("diff --git a/a.txt"));
        assert!(!m[0].patch.contains("veyra 1.0.0"));
    }
}
