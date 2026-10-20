//! Reading and writing Git configuration. Veyra's own settings live under `veyra.*`
//! in the same files, so `git config` and `veyra config` are interchangeable.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use gix::bstr::ByteSlice;

use crate::Repo;

/// Which configuration file to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `.git/config`
    Local,
    /// `~/.gitconfig` (or `$XDG_CONFIG_HOME/git/config` if that is what exists)
    Global,
}

/// A parsed `section[.subsection].name` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub section: String,
    pub subsection: Option<String>,
    pub name: String,
}

impl Key {
    pub fn parse(key: &str) -> Result<Key> {
        let first = key.find('.').ok_or_else(|| anyhow!("invalid key '{key}': expected section.name"))?;
        let last = key.rfind('.').expect("found one");
        let section = &key[..first];
        let name = &key[last + 1..];
        let valid_ident = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !valid_ident(section) || !valid_ident(name) || !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        {
            bail!("invalid key '{key}'");
        }
        let subsection = (first != last).then(|| key[first + 1..last].to_owned());
        Ok(Key { section: section.to_ascii_lowercase(), subsection, name: name.to_owned() })
    }

    pub fn full(&self) -> String {
        match &self.subsection {
            Some(sub) => format!("{}.{}.{}", self.section, sub, self.name),
            None => format!("{}.{}", self.section, self.name),
        }
    }
}

/// The path of the global configuration file.
pub fn global_config_path() -> Result<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))
        .map(|d| d.join("git").join("config"));
    let home = home_dir().map(|h| h.join(".gitconfig"));
    match (home, xdg) {
        (Some(home), _) if home.exists() => Ok(home),
        (_, Some(xdg)) if xdg.exists() => Ok(xdg),
        (Some(home), _) => Ok(home),
        (None, Some(xdg)) => Ok(xdg),
        (None, None) => bail!("could not determine the home directory for --global"),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Set `key = value` in the given file, creating the file if needed.
pub fn set_in_file(path: &std::path::Path, key: &str, value: &str) -> Result<()> {
    let key = Key::parse(key)?;
    let mut file = load_file(path)?;
    let mut section = file
        .section_mut_or_create_new(&key.section, key.subsection.as_deref().map(|s| s.as_bytes().as_bstr()))
        .with_context(|| format!("invalid section in key {}", key.full()))?;
    section.set(key.name.as_str(), value.as_bytes().as_bstr())?;
    write_file(path, &file)
}

/// Remove `key` from the given file. Returns whether it existed.
pub fn unset_in_file(path: &std::path::Path, key: &str) -> Result<bool> {
    let key = Key::parse(key)?;
    let mut file = load_file(path)?;
    let removed = match file.section_mut(&key.section, key.subsection.as_deref().map(|s| s.as_bytes().as_bstr())) {
        Ok(mut section) => section.remove(&key.name).is_some(),
        Err(_) => false,
    };
    if removed {
        write_file(path, &file)?;
    }
    Ok(removed)
}

/// Remove a whole section (e.g. `remote "origin"`). Returns whether it existed.
pub fn remove_section_in_file(path: &std::path::Path, section: &str, subsection: Option<&str>) -> Result<bool> {
    let mut file = load_file(path)?;
    let removed = file.remove_section(section, subsection.map(|s| s.as_bytes().as_bstr())).is_some();
    if removed {
        write_file(path, &file)?;
    }
    Ok(removed)
}

/// All `key=value` pairs in a file, in file order.
pub fn list_file(path: &std::path::Path) -> Result<Vec<(String, String)>> {
    let file = load_file(path)?;
    let mut out = Vec::new();
    for section in file.sections() {
        let header = section.header();
        let prefix = match header.subsection_name() {
            Some(sub) => format!("{}.{}", header.name().to_str_lossy().to_ascii_lowercase(), sub.to_str_lossy()),
            None => header.name().to_str_lossy().to_ascii_lowercase(),
        };
        let body = section.body();
        for name in body.value_names() {
            for value in body.values(&name) {
                out.push((format!("{prefix}.{}", name.to_ascii_lowercase()), value.to_str_lossy().into_owned()));
            }
        }
    }
    Ok(out)
}

fn load_file(path: &std::path::Path) -> Result<gix::config::File> {
    let meta = gix::config::file::Metadata::from(gix::config::Source::Local).at(path);
    if path.exists() {
        let bytes = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        gix::config::File::from_bytes_owned(&mut bytes.clone(), meta, Default::default())
            .with_context(|| format!("could not parse {}", path.display()))
    } else {
        Ok(gix::config::File::new(meta))
    }
}

fn write_file(path: &std::path::Path, file: &gix::config::File) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut lock = gix::lock::File::acquire_to_update_resource(
        path,
        gix::lock::acquire::Fail::AfterDurationWithBackoff(std::time::Duration::from_secs(2)),
        None,
        0,
    )
    .with_context(|| format!("could not lock {}", path.display()))?;
    file.write_to(&mut lock)?;
    lock.commit().map_err(|e| anyhow!("could not write {}: {}", path.display(), e.error))?;
    Ok(())
}

impl Repo {
    /// The effective (merged) value of `key`, as Git would resolve it.
    pub fn config_get(&self, key: &str) -> Option<String> {
        self.git.config_snapshot().string(key).map(|v| v.to_str_lossy().into_owned())
    }

    /// Effective boolean value of `key`.
    pub fn config_bool(&self, key: &str) -> Option<bool> {
        self.git.config_snapshot().boolean(key)
    }

    /// Effective integer value of `key` (supports `k`, `m`, `g` suffixes).
    pub fn config_int(&self, key: &str) -> Option<i64> {
        self.git.config_snapshot().integer(key)
    }

    /// All effective values of a multi-valued key.
    pub fn config_strings(&self, key: &str) -> Vec<String> {
        self.git
            .config_snapshot()
            .strings(key)
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.to_str_lossy().into_owned())
            .collect()
    }

    /// Path of the config file for `scope`.
    pub fn config_path(&self, scope: Scope) -> Result<PathBuf> {
        Ok(match scope {
            Scope::Local => self.common_dir().join("config"),
            Scope::Global => global_config_path()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_with_and_without_subsections() {
        assert_eq!(
            Key::parse("user.name").unwrap(),
            Key { section: "user".into(), subsection: None, name: "name".into() }
        );
        assert_eq!(
            Key::parse("remote.my.origin.url").unwrap(),
            Key { section: "remote".into(), subsection: Some("my.origin".into()), name: "url".into() }
        );
        assert!(Key::parse("nodot").is_err());
        assert!(Key::parse("user.").is_err());
        assert!(Key::parse(".name").is_err());
    }

    #[test]
    fn set_list_unset_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        set_in_file(&path, "user.name", "Ada").unwrap();
        set_in_file(&path, "veyra.checkpoint.auto", "true").unwrap();
        set_in_file(&path, "user.name", "Grace").unwrap();
        let all = list_file(&path).unwrap();
        assert_eq!(all, vec![("user.name".into(), "Grace".into()), ("veyra.checkpoint.auto".into(), "true".into())]);
        assert!(unset_in_file(&path, "user.name").unwrap());
        assert!(!unset_in_file(&path, "user.name").unwrap());
    }
}
