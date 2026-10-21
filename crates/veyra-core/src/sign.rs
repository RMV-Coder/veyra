//! Signing and verifying commits and tags, with Git's configuration (`gpg.format`,
//! `user.signingKey`, `gpg.ssh.allowedSignersFile`, `gpg.program`, …) and Git's external
//! programs (`gpg`, `gpgsm`, `ssh-keygen`), via gitoxide.

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;

use crate::Repo;

/// How to treat signing for one new commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignMode {
    /// Follow `commit.gpgSign`.
    #[default]
    Config,
    Always,
    Never,
}

/// The result of verifying one object's signature.
#[derive(Debug, Clone)]
pub struct Verification {
    pub id: ObjectId,
    /// `None` when the object isn't signed.
    pub outcome: Option<VerifyOutcome>,
}

#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    /// `openpgp`, `x509` or `ssh`.
    pub format: String,
    pub valid: bool,
    /// `good`, `bad`, `expired`, …
    pub status: String,
    pub trust: String,
    pub signer: Option<String>,
    pub key: Option<String>,
    /// The verifier's human-readable output.
    pub output: String,
}

fn to_outcome(o: gix::commit::verify::Outcome) -> VerifyOutcome {
    VerifyOutcome {
        format: format!("{:?}", o.format).to_lowercase(),
        valid: o.is_valid(),
        status: format!("{:?}", o.status).to_lowercase(),
        trust: format!("{:?}", o.trust_level).to_lowercase(),
        signer: o.signer.map(|s| s.to_str_lossy().into_owned()),
        key: o.key.map(|s| s.to_str_lossy().into_owned()),
        output: o.output.to_str_lossy().trim().to_owned(),
    }
}

impl Repo {
    /// Whether new commits should be signed under `mode`.
    pub fn should_sign(&self, mode: SignMode) -> bool {
        match mode {
            SignMode::Always => true,
            SignMode::Never => false,
            SignMode::Config => self.config_bool("commit.gpgSign").unwrap_or(false),
        }
    }

    /// Write a commit object, signing it first when `sign` is set.
    pub fn write_commit_object(&self, commit: gix::objs::Commit, sign: bool) -> Result<ObjectId> {
        let commit = if sign {
            let options = self.git.commit_signing_options().map_err(|e| anyhow!("cannot sign: {e}"))?;
            commit.sign(options).map_err(|e| anyhow!("signing the commit failed: {e}"))?
        } else {
            commit
        };
        Ok(self.git.write_object(&commit)?.detach())
    }

    /// Create an annotated tag object, optionally signed, and point `refs/tags/<name>` at it.
    pub fn create_tag(&self, name: &str, target: ObjectId, message: &str, sign: bool, force: bool) -> Result<ObjectId> {
        let tagger = self.git.committer().context("no tagger identity")??.to_owned()?;
        let kind = self.git.find_object(target)?.kind;
        let mut tag = gix::objs::Tag {
            target,
            target_kind: kind,
            name: name.into(),
            tagger: Some(tagger),
            message: message.into(),
            signature: None,
        };
        if sign {
            let options = self.git.commit_signing_options().map_err(|e| anyhow!("cannot sign: {e}"))?;
            tag = tag.sign(options).map_err(|e| anyhow!("signing the tag failed: {e}"))?;
        }
        let id = self.git.write_object(&tag)?.detach();
        let full = crate::refs::tag_full_name(name)?;
        let expected = if force { None } else { Some(None) };
        self.set_ref(&full, id, expected, "tag")?;
        Ok(id)
    }

    /// Verify a commit's signature.
    pub fn verify_commit(&self, id: ObjectId) -> Result<Verification> {
        let commit = self.git.find_commit(id).with_context(|| format!("{id} is not a commit"))?;
        let outcome = commit.verify_signature().map_err(|e| anyhow!("could not verify {}: {e}", crate::short(&id)))?;
        Ok(Verification { id, outcome: outcome.map(to_outcome) })
    }

    /// Verify an annotated tag's signature.
    pub fn verify_tag(&self, id: ObjectId) -> Result<Verification> {
        use gix::objs::signature::verify::Options;
        let object = self.git.find_object(id)?;
        if object.kind != gix::objs::Kind::Tag {
            bail!("{} is a lightweight tag (not a tag object), so it can't carry a signature", crate::short(&id));
        }
        let data = object.data.clone();
        let Some((signature, signed)) = gix::objs::TagRefIter::signature(&data) else {
            return Ok(Verification { id, outcome: None });
        };
        let signature = signature.data.as_bstr().to_owned();
        let format = gix::objs::signature::Format::from_signature(signature.as_ref())
            .ok_or_else(|| anyhow!("unsupported signature format on tag {}", crate::short(&id)))?;
        let config = self.git.config_snapshot();
        let program = |key: &str, default: &str| -> std::ffi::OsString {
            config.string(key).map(|v| v.to_str_lossy().into_owned()).unwrap_or_else(|| default.to_owned()).into()
        };
        let options = match format {
            gix::objs::signature::Format::OpenPgp => Options::OpenPgp {
                program: program("gpg.program", "gpg"),
                program_arguments: Vec::new(),
                environment: Vec::new(),
                minimum_trust: Default::default(),
            },
            gix::objs::signature::Format::X509 => Options::X509 {
                program: program("gpg.x509.program", "gpgsm"),
                program_arguments: Vec::new(),
                environment: Vec::new(),
                minimum_trust: Default::default(),
            },
            gix::objs::signature::Format::Ssh => {
                let allowed = config
                    .string("gpg.ssh.allowedSignersFile")
                    .map(|p| std::path::PathBuf::from(expand_home(&p.to_str_lossy())))
                    .context("gpg.ssh.allowedSignersFile must be configured to verify SSH signatures")?;
                let tagged = gix::objs::TagRef::from_bytes(&data, self.object_hash())?
                    .tagger()?
                    .map(|t| t.time())
                    .transpose()?
                    .unwrap_or_else(gix::date::Time::now_utc);
                Options::Ssh {
                    program: program("gpg.ssh.program", "ssh-keygen"),
                    program_arguments: Vec::new(),
                    environment: Vec::new(),
                    allowed_signers: allowed,
                    revocation_file: None,
                    verify_time: tagged,
                    minimum_trust: Default::default(),
                }
            }
        };
        let outcome =
            signed.verify(signature.as_ref(), options).map_err(|e| anyhow!("could not verify the tag: {e}"))?;
        Ok(Verification { id, outcome: Some(to_outcome(outcome)) })
    }
}

fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|_| path.to_owned()),
        None => path.to_owned(),
    }
}
