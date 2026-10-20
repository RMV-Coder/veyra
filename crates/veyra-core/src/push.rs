//! `push`: the client side of Git's `receive-pack` protocol, built on gitoxide's transport
//! and handshake (gitoxide itself has no push yet).
//!
//! Flow (see Git's `Documentation/gitprotocol-pack.txt`, section "Pushing Data To a Server"):
//!
//! 1. Handshake for `git-receive-pack` (protocol v0/v1 — v2 has no push). The server advertises
//!    its refs and capabilities. Authentication goes through the configured credential helpers.
//! 2. Plan: for every refspec, compare the remote's value with ours. Non-fast-forward updates are
//!    rejected unless forced; `--force-with-lease` checks the remote still has what we last fetched.
//! 3. Send `<old> <new> <ref>` command pkt-lines, a flush, then a packfile with every object the
//!    remote is missing.
//! 4. Read the `report-status` response and update remote-tracking refs for what was accepted.
//!
//! The packfile is written without deltas for now (correct, but larger than Git's). Delta
//! compression via `gix-pack`'s output pipeline is a planned optimisation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;

use crate::Repo;

/// One ref to update on the remote.
#[derive(Debug, Clone)]
pub struct PushSpec {
    /// Local source, resolved to an object (`None` deletes `dst`).
    pub src: Option<ObjectId>,
    /// Display name of the source (e.g. `main`).
    pub src_label: String,
    /// Full remote ref name, e.g. `refs/heads/main`.
    pub dst: String,
    /// Allow non-fast-forward updates (`+refspec` or `--force`).
    pub force: bool,
}

/// How to push.
#[derive(Debug, Clone, Default)]
pub struct PushOptions {
    /// Only overwrite a remote ref if it still points where our remote-tracking ref says.
    pub force_with_lease: bool,
    /// Plan and report, but send nothing.
    pub dry_run: bool,
}

/// What happened to one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefStatus {
    UpToDate,
    /// Accepted by the remote (or would be, on a dry run).
    Updated {
        forced: bool,
    },
    Created,
    Deleted,
    /// Refused locally before sending (e.g. non-fast-forward).
    RejectedLocally(String),
    /// Refused by the remote, with its reason.
    RejectedByRemote(String),
}

#[derive(Debug, Clone)]
pub struct RefResult {
    pub dst: String,
    pub src_label: String,
    pub old: Option<ObjectId>,
    pub new: Option<ObjectId>,
    pub status: RefStatus,
}

impl RefResult {
    pub fn is_success(&self) -> bool {
        !matches!(self.status, RefStatus::RejectedLocally(_) | RefStatus::RejectedByRemote(_))
    }
}

#[derive(Debug, Clone)]
pub struct PushOutcome {
    pub remote: String,
    pub url: String,
    pub results: Vec<RefResult>,
    pub objects_sent: usize,
    pub pack_bytes: u64,
    /// How many objects were sent as deltas.
    pub deltas: usize,
}

impl PushOutcome {
    pub fn all_succeeded(&self) -> bool {
        self.results.iter().all(RefResult::is_success)
    }
}

/// Writes through to `inner` while hashing everything written (for the pack trailer).
struct HashingWriter<W: Write> {
    inner: W,
    hasher: gix::hash::Hasher,
    written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Encode one pkt-line (`0006a\n` style: 4 hex digits of total length, then the data).
pub fn pkt_line(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}

/// Which objects to send, and good delta bases for them.
#[derive(Debug, Default)]
pub struct PackPlan {
    pub objects: Vec<ObjectId>,
    /// Object → the previous version of the same path (a delta base candidate).
    pub base_hints: HashMap<ObjectId, ObjectId>,
    /// Objects the remote is known to have (usable as bases of a thin pack).
    pub remote_has: HashSet<ObjectId>,
}

/// Blobs larger than this are sent whole (deltifying them costs too much memory and time).
const MAX_DELTA_SOURCE: usize = 32 * 1024 * 1024;
/// Longest allowed chain of deltas within one pack.
const MAX_DELTA_DEPTH: u32 = 16;

fn write_entry_header(w: &mut impl Write, type_code: u8, size: usize) -> std::io::Result<()> {
    // Type in bits 4-6 of the first byte, size as a little-endian base-128 varint.
    let mut size = size as u64;
    let mut header = Vec::with_capacity(10);
    let mut byte = (type_code << 4) | (size & 0x0f) as u8;
    size >>= 4;
    while size > 0 {
        header.push(byte | 0x80);
        byte = (size & 0x7f) as u8;
        size >>= 7;
    }
    header.push(byte);
    w.write_all(&header)
}

/// Write a version-2 packfile containing `objects` without deltas. Returns the bytes written.
pub fn write_pack(repo: &Repo, objects: &[ObjectId], out: impl Write) -> Result<u64> {
    let plan = PackPlan { objects: objects.to_vec(), ..Default::default() };
    write_pack_with_deltas(repo, &plan, out).map(|(bytes, _)| bytes)
}

/// Write a version-2 packfile for `plan`, storing blobs as `REF_DELTA`s against their previous
/// version when that is much smaller. Bases are either in this pack or on the remote (a "thin"
/// pack, which receive-pack completes with `index-pack --fix-thin`). Returns (bytes, deltas).
pub fn write_pack_with_deltas(repo: &Repo, plan: &PackPlan, out: impl Write) -> Result<(u64, usize)> {
    let mut w = HashingWriter { inner: out, hasher: gix::hash::hasher(repo.object_hash()), written: 0 };
    w.write_all(b"PACK")?;
    w.write_all(&2u32.to_be_bytes())?;
    w.write_all(&(u32::try_from(plan.objects.len()).context("too many objects for one pack")?).to_be_bytes())?;
    let level = repo.git.pack_compression().unwrap_or_default();
    let in_pack: HashSet<ObjectId> = plan.objects.iter().copied().collect();
    let mut depth: HashMap<ObjectId, u32> = HashMap::new();
    let mut deltas = 0usize;
    for id in &plan.objects {
        let object = repo.git.find_object(*id).with_context(|| format!("object {id} is missing locally"))?;
        let type_code: u8 = match object.kind {
            gix::objs::Kind::Commit => 1,
            gix::objs::Kind::Tree => 2,
            gix::objs::Kind::Blob => 3,
            gix::objs::Kind::Tag => 4,
        };

        // Try a delta against the hinted base.
        let mut delta_entry: Option<(ObjectId, Vec<u8>)> = None;
        if object.kind == gix::objs::Kind::Blob
            && object.data.len() <= MAX_DELTA_SOURCE
            && let Some(base_id) = plan.base_hints.get(id)
            && (plan.remote_has.contains(base_id) || in_pack.contains(base_id))
            && depth.get(base_id).copied().unwrap_or(0) < MAX_DELTA_DEPTH
            && let Ok(base) = repo.git.find_object(*base_id)
            && base.kind == gix::objs::Kind::Blob
            && base.data.len() <= MAX_DELTA_SOURCE
            && let Some(delta) = crate::delta::create(&base.data, &object.data)
            && delta.len() < object.data.len() / 2
        {
            debug_assert_eq!(crate::delta::apply(&base.data, &delta).as_deref(), Some(&object.data[..]));
            delta_entry = Some((*base_id, delta));
        }

        match delta_entry {
            Some((base_id, delta)) => {
                write_entry_header(&mut w, 7, delta.len())?;
                w.write_all(base_id.as_bytes())?;
                let mut deflate = gix::zlib::stream::deflate::Write::new(&mut w, level);
                deflate.write_all(&delta)?;
                deflate.flush()?;
                drop(deflate);
                depth.insert(*id, depth.get(&base_id).copied().unwrap_or(0) + 1);
                deltas += 1;
            }
            None => {
                write_entry_header(&mut w, type_code, object.data.len())?;
                let mut deflate = gix::zlib::stream::deflate::Write::new(&mut w, level);
                deflate.write_all(&object.data)?;
                deflate.flush()?;
                drop(deflate);
            }
        }
    }
    let digest = std::mem::replace(&mut w.hasher, gix::hash::hasher(repo.object_hash())).try_finalize()?;
    w.inner.write_all(digest.as_bytes())?;
    w.inner.flush()?;
    Ok((w.written + digest.as_bytes().len() as u64, deltas))
}

impl Repo {
    /// Objects reachable from `wants` that are not reachable from `haves` (approximated, like
    /// Git, by excluding everything in the trees of the boundary commits).
    pub fn objects_to_send(&self, wants: &[ObjectId], haves: &[ObjectId]) -> Result<Vec<ObjectId>> {
        Ok(self.pack_plan(wants, haves)?.objects)
    }

    /// Like [`Repo::objects_to_send`], plus delta-base hints for the pack writer.
    pub fn pack_plan(&self, wants: &[ObjectId], haves: &[ObjectId]) -> Result<PackPlan> {
        let mut out = Vec::new();
        let mut seen: HashSet<ObjectId> = HashSet::new();

        // Annotated tags among the wants travel as themselves.
        let mut commit_wants = Vec::new();
        for want in wants {
            let mut id = *want;
            loop {
                let object = self.git.find_object(id)?;
                match object.kind {
                    gix::objs::Kind::Tag => {
                        if !haves.contains(&id) && seen.insert(id) {
                            out.push(id);
                        }
                        id = object.into_tag().target_id()?.detach();
                    }
                    gix::objs::Kind::Commit => {
                        commit_wants.push(id);
                        break;
                    }
                    gix::objs::Kind::Tree | gix::objs::Kind::Blob => {
                        self.collect_tree_objects(id, &mut seen, &mut out)?;
                        break;
                    }
                }
            }
        }
        if commit_wants.is_empty() {
            return Ok(PackPlan { objects: out, ..Default::default() });
        }

        let hidden: Vec<ObjectId> = haves
            .iter()
            .copied()
            .filter(|h| self.git.has_object(h))
            .filter_map(|h| self.peel_to_commit(h).ok())
            .collect();
        let walk = self.git.rev_walk(commit_wants).with_hidden(hidden.iter().copied()).all()?;
        let mut new_commits = Vec::new();
        let mut new_set = HashSet::new();
        for info in walk {
            let info = info.map_err(|e| anyhow!("history traversal failed: {e}"))?;
            new_set.insert(info.id);
            new_commits.push(info.id);
        }

        // Everything in the trees of commits the remote already has (the boundary) is known there.
        let mut known: HashSet<ObjectId> = HashSet::new();
        let mut boundary: Vec<ObjectId> = Vec::new();
        for c in &new_commits {
            for p in self.commit_info(*c)?.parents {
                if !new_set.contains(&p) && self.git.has_object(p) {
                    boundary.push(p);
                }
            }
        }
        boundary.sort();
        boundary.dedup();
        for b in boundary {
            let tree = self.commit_tree_id(b)?;
            let mut sink = Vec::new();
            self.collect_tree_objects(tree, &mut known, &mut sink)?;
        }

        seen.extend(known.iter().copied());
        let mut base_hints = HashMap::new();
        for c in &new_commits {
            if seen.insert(*c) {
                out.push(*c);
            }
            let info = self.commit_info(*c)?;
            self.collect_tree_objects(info.tree, &mut seen, &mut out)?;
            // The previous version of each changed path is a natural delta base.
            if let Some(parent) = info.parents.first().filter(|p| self.git.has_object(*p)) {
                let old_tree = self.git.find_tree(self.commit_tree_id(*parent)?)?;
                let new_tree = self.git.find_tree(info.tree)?;
                if let Ok(changes) = self.git.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None) {
                    use gix::object::tree::diff::ChangeDetached as C;
                    for change in changes {
                        match change {
                            C::Modification { previous_id, id, .. } => {
                                base_hints.entry(id).or_insert(previous_id);
                            }
                            C::Rewrite { source_id, id, .. } => {
                                base_hints.entry(id).or_insert(source_id);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(PackPlan { objects: out, base_hints, remote_has: known })
    }

    /// Add `tree` and everything below it to `out`, skipping objects already in `seen`.
    fn collect_tree_objects(
        &self,
        tree: ObjectId,
        seen: &mut HashSet<ObjectId>,
        out: &mut Vec<ObjectId>,
    ) -> Result<()> {
        if !seen.insert(tree) {
            return Ok(());
        }
        out.push(tree);
        let object = self.git.find_object(tree)?;
        if object.kind != gix::objs::Kind::Tree {
            return Ok(());
        }
        let tree_obj = object.into_tree();
        let entries: Vec<(EntryKind, ObjectId)> =
            tree_obj.decode()?.entries.iter().map(|e| (e.mode.kind(), e.oid.to_owned())).collect();
        for (kind, id) in entries {
            match kind {
                EntryKind::Tree => self.collect_tree_objects(id, seen, out)?,
                EntryKind::Commit => {} // submodule commits live in other repositories
                _ => {
                    if seen.insert(id) {
                        out.push(id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Push `specs` to `remote_name`.
    ///
    /// `before_send` sees the planned updates right before anything is sent (used for the
    /// `pre-push` hook); returning an error aborts the push.
    pub fn push(
        &self,
        remote_name: &str,
        specs: &[PushSpec],
        options: &PushOptions,
        before_send: &mut dyn FnMut(&str, &[RefResult]) -> Result<()>,
    ) -> Result<PushOutcome> {
        use gix::protocol::transport::client::blocking_io::{Transport, connect};
        use gix::protocol::transport::client::{MessageKind, WriteMode};

        let remote = self.git.find_remote(remote_name).map_err(|e| anyhow!("no such remote '{remote_name}': {e}"))?;
        let url = remote
            .url(gix::remote::Direction::Push)
            .ok_or_else(|| anyhow!("remote '{remote_name}' has no URL"))?
            .to_owned();
        let url_display = url.to_bstring().to_string();

        let ssh =
            if url.scheme == gix::url::Scheme::Ssh { self.git.ssh_connect_options()? } else { Default::default() };
        let mut transport = connect::connect(
            url.clone(),
            connect::Options { version: gix::protocol::transport::Protocol::V1, ssh, trace: false },
        )
        .with_context(|| format!("could not connect to {url_display}"))?;

        let (mut cascade, _, prompt) = self.git.config_snapshot().credential_helpers(url.clone())?;
        let authenticate = move |action| cascade.invoke(action, prompt.clone());
        let handshake = gix::protocol::handshake(
            &mut transport,
            gix::protocol::transport::Service::ReceivePack,
            authenticate,
            Vec::new(),
            &mut gix::progress::Discard,
        )
        .with_context(|| format!("could not talk to {url_display} (git-receive-pack)"))?;

        let mut remote_refs: BTreeMap<String, ObjectId> = BTreeMap::new();
        for r in handshake.refs.unwrap_or_default() {
            use gix::protocol::handshake::Ref;
            match r {
                Ref::Direct { full_ref_name, object } => {
                    remote_refs.insert(full_ref_name.to_string(), object);
                }
                Ref::Peeled { full_ref_name, tag, .. } => {
                    remote_refs.insert(full_ref_name.to_string(), tag);
                }
                Ref::Symbolic { .. } | Ref::Unborn { .. } => {}
            }
        }
        let caps = &handshake.capabilities;
        let has_cap = |name: &str| caps.contains(name);

        // ---- plan -----------------------------------------------------------------------
        let mut results = Vec::new();
        let mut commands: Vec<(Option<ObjectId>, Option<ObjectId>, String)> = Vec::new();
        for spec in specs {
            let old = remote_refs.get(&spec.dst).copied();
            let mut result = RefResult {
                dst: spec.dst.clone(),
                src_label: spec.src_label.clone(),
                old,
                new: spec.src,
                status: RefStatus::UpToDate,
            };
            match (old, spec.src) {
                (None, None) => {
                    result.status = RefStatus::RejectedLocally("remote ref does not exist".into());
                }
                (Some(o), Some(n)) if o == n => {}
                (_, None) => {
                    if !has_cap("delete-refs") {
                        result.status = RefStatus::RejectedLocally("the remote does not allow deleting refs".into());
                    } else {
                        result.status = RefStatus::Deleted;
                        commands.push((old, None, spec.dst.clone()));
                    }
                }
                (None, Some(n)) => {
                    result.status = RefStatus::Created;
                    commands.push((None, Some(n), spec.dst.clone()));
                }
                (Some(o), Some(n)) => {
                    let lease = if options.force_with_lease {
                        let tracking = tracking_ref(remote_name, &spec.dst);
                        let expected = match &tracking {
                            Some(t) => self.ref_target(t)?,
                            None => None,
                        };
                        Some(expected == Some(o))
                    } else {
                        None
                    };
                    let fast_forward = self.git.has_object(o) && self.is_ancestor(o, n).unwrap_or(false);
                    let forced_ok = spec.force && lease != Some(false);
                    if fast_forward && lease != Some(false) {
                        result.status = RefStatus::Updated { forced: false };
                        commands.push((Some(o), Some(n), spec.dst.clone()));
                    } else if forced_ok || (options.force_with_lease && lease == Some(true)) {
                        result.status = RefStatus::Updated { forced: true };
                        commands.push((Some(o), Some(n), spec.dst.clone()));
                    } else if lease == Some(false) {
                        result.status =
                            RefStatus::RejectedLocally("stale info: the remote changed since your last fetch".into());
                    } else if !self.git.has_object(o) {
                        result.status =
                            RefStatus::RejectedLocally("fetch first: the remote has commits you don't have".into());
                    } else {
                        result.status = RefStatus::RejectedLocally(
                            "non-fast-forward: pull (or merge) first, or use --force-with-lease".into(),
                        );
                    }
                }
            }
            results.push(result);
        }

        let mut outcome = PushOutcome {
            deltas: 0,
            remote: remote_name.to_owned(),
            url: url_display.clone(),
            results,
            objects_sent: 0,
            pack_bytes: 0,
        };
        if commands.is_empty() || options.dry_run {
            // Tell receive-pack there is nothing to do (a lone flush), so it exits right away
            // instead of waiting for commands.
            if let Ok(request) = transport.request(WriteMode::Binary, MessageKind::Flush, false) {
                let (mut writer, _reader) = request.into_parts();
                let _ = writer.write_all(b"0000");
                let _ = writer.flush();
            }
            return Ok(outcome);
        }

        if let Err(err) = before_send(&url_display, &outcome.results) {
            if let Ok(request) = transport.request(WriteMode::Binary, MessageKind::Flush, false) {
                let (mut writer, _reader) = request.into_parts();
                let _ = writer.write_all(b"0000");
                let _ = writer.flush();
            }
            return Err(err);
        }

        // ---- objects --------------------------------------------------------------------
        let wants: Vec<ObjectId> = commands.iter().filter_map(|(_, new, _)| *new).collect();
        let haves: Vec<ObjectId> = remote_refs.values().copied().collect();
        let plan = if wants.is_empty() { PackPlan::default() } else { self.pack_plan(&wants, &haves)? };

        // ---- send -----------------------------------------------------------------------
        let null = ObjectId::null(self.object_hash());
        let request = transport
            .request(WriteMode::Binary, MessageKind::Flush, false)
            .with_context(|| format!("could not start the push to {url_display}"))?;
        let (mut writer, mut reader) = request.into_parts();
        let mut capabilities = vec!["report-status".to_owned()];
        if commands.len() > 1 && has_cap("atomic") {
            capabilities.push("atomic".into());
        }
        if has_cap("quiet") {
            capabilities.push("quiet".into());
        }
        capabilities.push(format!("agent=veyra/{}", env!("CARGO_PKG_VERSION")));
        for (i, (old, new, name)) in commands.iter().enumerate() {
            let mut line = format!("{} {} {}", old.unwrap_or(null), new.unwrap_or(null), name).into_bytes();
            if i == 0 {
                line.push(0);
                line.extend_from_slice(capabilities.join(" ").as_bytes());
            }
            line.push(b'\n');
            writer.write_all(&pkt_line(&line))?;
        }
        writer.write_all(b"0000")?;
        if !wants.is_empty() {
            let (bytes, deltas) = write_pack_with_deltas(self, &plan, &mut writer)?;
            outcome.pack_bytes = bytes;
            outcome.objects_sent = plan.objects.len();
            outcome.deltas = deltas;
        }
        writer.flush()?;
        drop(writer);

        // ---- report-status ----------------------------------------------------------------
        let mut statuses: BTreeMap<String, Result<(), String>> = BTreeMap::new();
        let mut line = String::new();
        let mut unpack_error = None;
        loop {
            line.clear();
            let n =
                reader.readline_str(&mut line).context("the remote closed the connection before reporting status")?;
            if n == 0 {
                break;
            }
            let text = line.trim_end_matches('\n');
            if let Some(rest) = text.strip_prefix("unpack ") {
                if rest != "ok" {
                    unpack_error = Some(rest.to_owned());
                }
            } else if let Some(r) = text.strip_prefix("ok ") {
                statuses.insert(r.to_owned(), Ok(()));
            } else if let Some(rest) = text.strip_prefix("ng ") {
                let (r, reason) = rest.split_once(' ').unwrap_or((rest, "rejected"));
                statuses.insert(r.to_owned(), Err(reason.to_owned()));
            }
        }
        if let Some(err) = unpack_error {
            bail!("the remote failed to unpack the objects: {err}");
        }

        // ---- record ---------------------------------------------------------------------
        let mut tracking_edits = Vec::new();
        for result in &mut outcome.results {
            if matches!(result.status, RefStatus::UpToDate | RefStatus::RejectedLocally(_)) {
                continue;
            }
            if let Some(Err(reason)) = statuses.get(&result.dst) {
                result.status = RefStatus::RejectedByRemote(reason.clone());
                continue;
            }
            if let Some(tracking) = tracking_ref(remote_name, &result.dst) {
                tracking_edits.push((tracking, result.new));
            }
        }
        self.set_refs(&tracking_edits, &format!("update by push to {remote_name}"))?;
        Ok(outcome)
    }
}

/// `refs/heads/x` on remote `origin` is tracked locally as `refs/remotes/origin/x`.
pub fn tracking_ref(remote: &str, dst: &str) -> Option<String> {
    dst.strip_prefix("refs/heads/").map(|b| format!("refs/remotes/{remote}/{b}"))
}

/// Parse a refspec like `main`, `main:release`, `+main`, `:old-branch`, `HEAD`, `v1.0`.
/// Returns (source spec or `None` for delete, destination full ref, force).
pub fn parse_refspec(repo: &Repo, spec: &str) -> Result<(Option<String>, String, bool)> {
    let (force, spec) = match spec.strip_prefix('+') {
        Some(s) => (true, s),
        None => (false, spec),
    };
    let (src, dst) = match spec.split_once(':') {
        Some((s, d)) => (s, Some(d)),
        None => (spec, None),
    };
    if src.is_empty() {
        let dst = dst.filter(|d| !d.is_empty()).context("empty refspec")?;
        return Ok((None, full_remote_name(repo, dst, dst)?, force));
    }
    let src_full = if src == "HEAD" {
        match repo.head_state()? {
            crate::HeadState::Branch { name, .. } => name.to_str_lossy().into_owned(),
            crate::HeadState::Detached(_) => {
                let dst = dst.context("HEAD is detached; name the destination, e.g. HEAD:refs/heads/topic")?;
                return Ok((Some("HEAD".into()), full_remote_name(repo, dst, dst)?, force));
            }
        }
    } else if repo.ref_target(&format!("refs/heads/{src}"))?.is_some() {
        format!("refs/heads/{src}")
    } else if repo.ref_target(&format!("refs/tags/{src}"))?.is_some() {
        format!("refs/tags/{src}")
    } else if src.starts_with("refs/") {
        src.to_owned()
    } else {
        // A commit-ish (e.g. a hash) requires an explicit destination.
        let dst = dst.with_context(|| {
            format!("'{src}' is not a branch or tag; give a destination like {src}:refs/heads/<name>")
        })?;
        repo.rev_parse(src)?;
        return Ok((Some(src.to_owned()), full_remote_name(repo, dst, dst)?, force));
    };
    let dst_full = match dst {
        Some(d) => full_remote_name(repo, d, &src_full)?,
        None => src_full.clone(),
    };
    Ok((Some(src_full), dst_full, force))
}

/// Expand a short destination like `main` into `refs/heads/main` (or `refs/tags/x` when the
/// source is a tag).
fn full_remote_name(_repo: &Repo, dst: &str, src_full: &str) -> Result<String> {
    if dst.starts_with("refs/") {
        return Ok(dst.to_owned());
    }
    if src_full.starts_with("refs/tags/") {
        return Ok(format!("refs/tags/{dst}"));
    }
    Ok(format!("refs/heads/{dst}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkt_lines_have_a_hex_length_prefix() {
        assert_eq!(pkt_line(b"a\n"), b"0006a\n");
        assert_eq!(pkt_line(b""), b"0004");
    }

    #[test]
    fn tracking_refs() {
        assert_eq!(tracking_ref("origin", "refs/heads/main").as_deref(), Some("refs/remotes/origin/main"));
        assert_eq!(tracking_ref("origin", "refs/tags/v1"), None);
    }
}
