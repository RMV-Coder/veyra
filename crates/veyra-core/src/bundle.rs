//! Git bundles (`git bundle`): refs plus a pack in one file, for moving history without a server.
//!
//! Format v2: `# v2 git bundle`, prerequisite lines (`-<id> <subject>`: commits the receiver must
//! already have), ref lines (`<id> <refname>`), a blank line, then a pack. Veyra writes complete
//! (non-thin) packs, which every Git version reads.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;

use crate::Repo;

const SIGNATURE: &str = "# v2 git bundle";

/// A bundle's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleHeader {
    pub prerequisites: Vec<(ObjectId, String)>,
    pub refs: Vec<(ObjectId, String)>,
}

/// Split a bundle into its header and the offset where the pack starts.
pub fn read_header(data: &[u8]) -> Result<(BundleHeader, usize)> {
    let mut pos = 0;
    let mut next_line = || -> Option<&[u8]> {
        let rest = data.get(pos..)?;
        let end = rest.iter().position(|b| *b == b'\n')?;
        pos += end + 1;
        Some(&rest[..end])
    };
    let first = next_line().context("not a bundle (empty)")?;
    if first != SIGNATURE.as_bytes() {
        bail!("not a v2 git bundle");
    }
    let mut header = BundleHeader { prerequisites: Vec::new(), refs: Vec::new() };
    loop {
        let line = next_line().context("truncated bundle header")?;
        if line.is_empty() {
            break;
        }
        let text = std::str::from_utf8(line).context("bundle header is not UTF-8")?;
        if let Some(rest) = text.strip_prefix('-') {
            let (id, comment) = rest.split_once(' ').unwrap_or((rest, ""));
            header.prerequisites.push((ObjectId::from_hex(id.as_bytes())?, comment.to_owned()));
        } else {
            let (id, name) = text.split_once(' ').context("bad ref line in bundle")?;
            header.refs.push((ObjectId::from_hex(id.as_bytes())?, name.to_owned()));
        }
    }
    if !data[pos..].starts_with(b"PACK") {
        bail!("the bundle has no pack data");
    }
    Ok((header, pos))
}

impl Repo {
    /// Write a bundle with `refs` (full names) and everything they reach, minus what `exclude`
    /// reaches. Returns the header written.
    pub fn bundle_create(
        &self,
        path: &Path,
        refs: &[(String, ObjectId)],
        exclude: &[ObjectId],
    ) -> Result<BundleHeader> {
        if refs.is_empty() {
            bail!("nothing to bundle: no refs given");
        }
        let wants: Vec<ObjectId> = refs.iter().map(|(_, id)| *id).collect();
        let plan = self.pack_plan(&wants, exclude)?;
        // Prerequisites: excluded commits that included commits build on.
        let included: HashSet<ObjectId> = plan.objects.iter().copied().collect();
        let mut prerequisites = Vec::new();
        let mut seen = HashSet::new();
        for id in &plan.objects {
            if self.git.find_object(*id)?.kind != gix::object::Kind::Commit {
                continue;
            }
            for parent in self.commit_info(*id)?.parents {
                if !included.contains(&parent) && seen.insert(parent) {
                    prerequisites.push((parent, self.commit_info(parent)?.summary().to_owned()));
                }
            }
        }
        let header = BundleHeader { prerequisites, refs: refs.iter().map(|(n, id)| (*id, n.clone())).collect() };
        let mut out = format!("{SIGNATURE}\n").into_bytes();
        for (id, comment) in &header.prerequisites {
            out.extend_from_slice(format!("-{id} {comment}\n").as_bytes());
        }
        for (id, name) in &header.refs {
            out.extend_from_slice(format!("{id} {name}\n").as_bytes());
        }
        out.push(b'\n');
        crate::push::write_pack(self, &plan.objects, &mut out)?;
        std::fs::write(path, out).with_context(|| format!("could not write {}", path.display()))?;
        Ok(header)
    }

    /// Check that a bundle is well-formed and that this repository has its prerequisites.
    pub fn bundle_verify(&self, path: &Path) -> Result<BundleHeader> {
        let data = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        let (header, _) = read_header(&data)?;
        let missing: Vec<String> = header
            .prerequisites
            .iter()
            .filter(|(id, _)| !self.git.has_object(*id))
            .map(|(id, c)| format!("{} {c}", crate::short(id)))
            .collect();
        if !missing.is_empty() {
            bail!("this repository lacks the bundle's prerequisite commits:\n  {}", missing.join("\n  "));
        }
        Ok(header)
    }

    /// Store a bundle's objects in this repository (refs are left alone, like `git bundle
    /// unbundle`). Returns the bundle's refs.
    pub fn bundle_unbundle(&self, path: &Path) -> Result<Vec<(ObjectId, String)>> {
        let header = self.bundle_verify(path)?;
        let data = std::fs::read(path)?;
        let (_, offset) = read_header(&data)?;
        let pack_dir = self.common_dir().join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        let never = std::sync::atomic::AtomicBool::new(false);
        let outcome = gix::odb::pack::Bundle::write_to_directory(
            &mut std::io::BufReader::new(&data[offset..]),
            Some(&pack_dir),
            &mut gix::progress::Discard,
            &never,
            Some(self.git.objects.clone().into_arc()?),
            self.object_hash(),
            Default::default(),
        )
        .map_err(|e| anyhow!("could not store the bundle's objects: {e}"))?;
        if let Some(keep) = &outcome.keep_path {
            let _ = std::fs::remove_file(keep);
        }
        Ok(header.refs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parsing() {
        let id = "1234567890123456789012345678901234567890";
        let data = format!("{SIGNATURE}\n-{id} base commit\n{id} refs/heads/main\n\nPACK....");
        let (h, at) = read_header(data.as_bytes()).unwrap();
        assert_eq!(h.prerequisites[0].1, "base commit");
        assert_eq!(h.refs[0].1, "refs/heads/main");
        assert!(data[at..].starts_with("PACK"));
        assert!(read_header(b"# v3 git bundle\n\n").is_err());
    }
}
