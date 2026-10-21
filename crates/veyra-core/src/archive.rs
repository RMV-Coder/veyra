//! `veyra archive`: a commit's tree as tar, tar.gz or zip, like `git archive`.

use std::io::Write;

use anyhow::{Result, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;

use crate::Repo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Tar,
    TarGz,
    Zip,
}

impl ArchiveFormat {
    pub fn parse(s: &str) -> Option<ArchiveFormat> {
        Some(match s {
            "tar" => ArchiveFormat::Tar,
            "tgz" | "tar.gz" => ArchiveFormat::TarGz,
            "zip" => ArchiveFormat::Zip,
            _ => return None,
        })
    }

    /// Guess from an output file name.
    pub fn from_file_name(name: &str) -> Option<ArchiveFormat> {
        let n = name.to_ascii_lowercase();
        if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
            Some(ArchiveFormat::TarGz)
        } else if n.ends_with(".tar") {
            Some(ArchiveFormat::Tar)
        } else if n.ends_with(".zip") {
            Some(ArchiveFormat::Zip)
        } else {
            None
        }
    }
}

struct Entry {
    path: String,
    mode: u32,
    data: Vec<u8>,
    /// Symlink target.
    link: Option<String>,
}

/// Raw DEFLATE (gzip and zip need it without the zlib wrapper).
fn deflate_raw(data: &[u8]) -> Result<Vec<u8>> {
    let mut zlib = Vec::with_capacity(data.len() / 2 + 16);
    {
        let mut w = gix::zlib::stream::deflate::Write::new(&mut zlib, gix::zlib::Compression::default());
        w.write_all(data)?;
        w.flush()?;
    }
    if zlib.len() < 6 {
        bail!("compression failed");
    }
    Ok(zlib[2..zlib.len() - 4].to_vec()) // strip the 2-byte header and the Adler-32 trailer
}

fn octal(field: &mut [u8], value: u64) {
    let s = format!("{:0width$o}", value, width = field.len() - 1);
    field[..s.len()].copy_from_slice(s.as_bytes());
}

fn tar_header(name: &str, mode: u32, size: u64, mtime: i64, kind: u8, link: &str) -> [u8; 512] {
    let mut h = [0u8; 512];
    let name_bytes = name.as_bytes();
    h[..name_bytes.len().min(100)].copy_from_slice(&name_bytes[..name_bytes.len().min(100)]);
    octal(&mut h[100..108], mode as u64);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], mtime.max(0) as u64);
    h[148..156].copy_from_slice(b"        ");
    h[156] = kind;
    let link = link.as_bytes();
    h[157..157 + link.len().min(100)].copy_from_slice(&link[..link.len().min(100)]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[265..269].copy_from_slice(b"root");
    h[297..301].copy_from_slice(b"root");
    let sum: u32 = h.iter().map(|b| *b as u32).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    h
}

/// A pax extended header record: `"<len> key=value\n"` where len counts the whole record.
fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while format!("{len}{body}").len() != len {
        len += 1;
    }
    format!("{len}{body}")
}

fn tar_bytes(entries: &[Entry], mtime: i64, comment: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let pad = |out: &mut Vec<u8>| {
        let rem = out.len() % 512;
        if rem != 0 {
            out.extend(std::iter::repeat_n(0u8, 512 - rem));
        }
    };
    // Global header with the commit id, like `git archive` (`git get-tar-commit-id` reads it).
    let global = pax_record("comment", comment);
    out.extend_from_slice(&tar_header("pax_global_header", 0o666, global.len() as u64, mtime, b'g', ""));
    out.extend_from_slice(global.as_bytes());
    pad(&mut out);
    for e in entries {
        let link = e.link.as_deref().unwrap_or("");
        if e.path.len() > 100 || link.len() > 100 {
            let mut pax = pax_record("path", &e.path);
            if link.len() > 100 {
                pax.push_str(&pax_record("linkpath", link));
            }
            out.extend_from_slice(&tar_header("pax_header", 0o666, pax.len() as u64, mtime, b'x', ""));
            out.extend_from_slice(pax.as_bytes());
            pad(&mut out);
        }
        let short: String = e.path.chars().take(100).collect();
        let (kind, size) = if e.link.is_some() { (b'2', 0) } else { (b'0', e.data.len() as u64) };
        out.extend_from_slice(&tar_header(&short, e.mode, size, mtime, kind, link));
        if e.link.is_none() {
            out.extend_from_slice(&e.data);
            pad(&mut out);
        }
    }
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

fn dos_time(seconds: i64) -> (u16, u16) {
    let Ok(ts) = jiff::Timestamp::from_second(seconds) else { return (0, 0x21) };
    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC).datetime();
    let year = (dt.year() as i32 - 1980).clamp(0, 127) as u16;
    let time = ((dt.hour() as u16) << 11) | ((dt.minute() as u16) << 5) | (dt.second() as u16 / 2);
    let date = (year << 9) | ((dt.month() as u16) << 5) | dt.day() as u16;
    (time, date)
}

fn zip_bytes(entries: &[Entry], mtime: i64, comment: &str) -> Result<Vec<u8>> {
    let (time, date) = dos_time(mtime);
    let mut out = Vec::new();
    let mut central = Vec::new();
    for e in entries {
        let data = match &e.link {
            Some(l) => l.as_bytes().to_vec(),
            None => e.data.clone(),
        };
        let crc = crc32fast::hash(&data);
        let compressed = deflate_raw(&data)?;
        let (method, body) = if compressed.len() < data.len() { (8u16, compressed) } else { (0u16, data.clone()) };
        if body.len() > u32::MAX as usize || out.len() > u32::MAX as usize {
            bail!("the archive is too large for zip (no zip64 support); use tar");
        }
        let offset = out.len() as u32;
        let name = e.path.as_bytes();
        // Local file header.
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&time.to_le_bytes());
        out.extend_from_slice(&date.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&body);
        // Central directory entry; Unix mode in the external attributes.
        let unix_mode: u32 = if e.link.is_some() { 0o120777 } else { 0o100000 | e.mode };
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&((3u16 << 8) | 20).to_le_bytes()); // made by Unix
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(body.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 8]); // extra len, comment len, disk, internal attrs
        central.extend_from_slice(&(unix_mode << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    out.extend_from_slice(comment.as_bytes());
    Ok(out)
}

impl Repo {
    /// Archive the tree of `commit` (optionally only `paths`), each entry under `prefix`.
    pub fn archive(&self, commit: ObjectId, format: ArchiveFormat, prefix: &str, paths: &[String]) -> Result<Vec<u8>> {
        let info = self.commit_info(commit)?;
        let flat = self.flat_from_tree(info.tree)?;
        let mut entries = Vec::new();
        for (path, e) in &flat {
            let p = path.to_str_lossy();
            if !paths.is_empty()
                && !paths.iter().any(|w| p == w.as_str() || p.starts_with(&format!("{}/", w.trim_end_matches('/'))))
            {
                continue;
            }
            let full = format!("{prefix}{p}");
            match e.kind {
                EntryKind::Blob => {
                    entries.push(Entry { path: full, mode: 0o644, data: self.read_blob(e.id)?, link: None })
                }
                EntryKind::BlobExecutable => {
                    entries.push(Entry { path: full, mode: 0o755, data: self.read_blob(e.id)?, link: None })
                }
                EntryKind::Link => {
                    let target = String::from_utf8_lossy(&self.read_blob(e.id)?).into_owned();
                    entries.push(Entry { path: full, mode: 0o777, data: Vec::new(), link: Some(target) });
                }
                EntryKind::Commit | EntryKind::Tree => {} // submodules aren't included, like git
            }
        }
        if entries.is_empty() && !paths.is_empty() {
            bail!("no files match {}", paths.join(", "));
        }
        let comment = commit.to_string();
        let mtime = info.committer_time;
        Ok(match format {
            ArchiveFormat::Tar => tar_bytes(&entries, mtime, &comment),
            ArchiveFormat::TarGz => {
                let tar = tar_bytes(&entries, mtime, &comment);
                let mut gz = vec![0x1f, 0x8b, 8, 0];
                gz.extend_from_slice(&(mtime.max(0) as u32).to_le_bytes());
                gz.extend_from_slice(&[0, 255]); // no extra flags, unknown OS
                gz.extend_from_slice(&deflate_raw(&tar)?);
                gz.extend_from_slice(&crc32fast::hash(&tar).to_le_bytes());
                gz.extend_from_slice(&(tar.len() as u32).to_le_bytes());
                gz
            }
            ArchiveFormat::Zip => zip_bytes(&entries, mtime, &comment)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_records_count_their_own_length() {
        let r = pax_record("path", &"x".repeat(150));
        let len: usize = r.split(' ').next().unwrap().parse().unwrap();
        assert_eq!(len, r.len());
    }

    #[test]
    fn raw_deflate_round_trips_through_zlib() {
        let data = b"hello hello hello hello".repeat(20);
        let raw = deflate_raw(&data).unwrap();
        assert!(raw.len() < data.len());
    }
}
