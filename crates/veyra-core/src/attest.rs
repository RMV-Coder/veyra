//! Attestations: verifiable evidence about commits (test runs, reviews, builds), stored as
//! [in-toto v1 Statements](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md)
//! in Git notes under `refs/notes/veyra/attestations` — so they travel with `push`/`fetch`
//! of that ref and never alter commit ids. Statements can be signed with the same SSH or
//! OpenPGP key used for commits (`gpg.format`, `user.signingKey`).

use std::io::Write;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Repo;
use crate::flat::FlatEntry;

pub const NOTES_REF: &str = "refs/notes/veyra/attestations";
/// SSH signature namespace. Git's own (`git`), so existing `allowed_signers` files (which
/// usually restrict keys to `namespaces="git"`) work unchanged. The signed payload is an
/// in-toto JSON statement, which cannot be confused with a commit or tag object.
const SSH_NAMESPACE: &str = "git";

/// Predicate types Veyra produces.
pub const TEST_PREDICATE: &str = "https://veyra.dev/attestation/test-result/v1";
pub const REVIEW_PREDICATE: &str = "https://veyra.dev/attestation/review/v1";

/// One stored attestation: the statement plus an optional detached signature over its JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub statement: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<EnvelopeSignature>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnvelopeSignature {
    /// `ssh` or `openpgp`.
    pub format: String,
    /// Who signed (committer email), used as the SSH principal.
    pub signer: String,
    /// Armored signature.
    pub value: String,
}

/// Result of checking one envelope's signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureCheck {
    Unsigned,
    Good,
    Bad(String),
}

impl Envelope {
    pub fn predicate_type(&self) -> &str {
        self.statement["predicateType"].as_str().unwrap_or("")
    }

    /// The commit this statement is about.
    pub fn subject_commit(&self) -> Option<String> {
        self.statement["subject"][0]["digest"]["gitCommit"].as_str().map(str::to_owned)
    }

    fn payload(&self) -> Vec<u8> {
        serde_json::to_vec(&self.statement).expect("JSON values serialize")
    }
}

/// Build an in-toto v1 Statement about `commit`.
pub fn statement(commit: ObjectId, predicate_type: &str, predicate: Value) -> Value {
    json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "git-commit", "digest": { "gitCommit": commit.to_string() } }],
        "predicateType": predicate_type,
        "predicate": predicate,
    })
}

impl Repo {
    /// All attestations recorded for `commit`.
    pub fn attestations(&self, commit: ObjectId) -> Result<Vec<Envelope>> {
        let Some(notes) = self.ref_target(NOTES_REF)? else { return Ok(Vec::new()) };
        let flat = self.flat_from_commit(notes)?;
        let hex = commit.to_string();
        // Notes trees may be fanned out (`ab/cdef…`); match on the path with slashes removed.
        let Some((_, entry)) = flat.iter().find(|(p, _)| p.to_str_lossy().replace('/', "") == hex) else {
            return Ok(Vec::new());
        };
        let text = self.read_blob(entry.id)?;
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_slice::<Envelope>(l).with_context(|| format!("corrupt attestation note for {hex}"))
            })
            .collect()
    }

    /// Record `statement` for `commit`, signing it when `sign` is set.
    pub fn add_attestation(&self, commit: ObjectId, statement: Value, sign: bool) -> Result<Envelope> {
        let mut envelope = Envelope { statement, signature: None };
        if sign {
            envelope.signature = Some(self.sign_payload(&envelope.payload())?);
        }
        let previous = self.ref_target(NOTES_REF)?;
        let mut flat = match previous {
            Some(p) => self.flat_from_commit(p)?,
            None => Default::default(),
        };
        let hex = commit.to_string();
        let existing =
            flat.iter().find(|(p, _)| p.to_str_lossy().replace('/', "") == hex).map(|(p, e)| (p.clone(), *e));
        let mut content = match &existing {
            Some((_, e)) => self.read_blob(e.id)?,
            None => Vec::new(),
        };
        if !content.is_empty() && !content.ends_with(b"\n") {
            content.push(b'\n');
        }
        content.extend_from_slice(&serde_json::to_vec(&envelope)?);
        content.push(b'\n');
        let blob = self.write_blob(&content)?;
        let path = existing.map(|(p, _)| p).unwrap_or_else(|| BString::from(hex.as_str()));
        flat.insert(path, FlatEntry::new(EntryKind::Blob, blob));
        let tree = self.write_flat(&flat)?;
        let signature = self.git.committer().context("no identity")??.to_owned()?;
        let note = gix::objs::Commit {
            message: format!("veyra attest {} for {}\n", envelope.predicate_type(), crate::short(&commit))
                .as_str()
                .into(),
            tree,
            author: signature.clone(),
            committer: signature,
            encoding: None,
            parents: previous.into_iter().collect(),
            extra_headers: Default::default(),
        };
        let id = self.write_commit_object(note, false)?;
        self.set_ref(NOTES_REF, id, Some(previous), "veyra attest")?;
        Ok(envelope)
    }

    fn signing_format(&self) -> String {
        self.config_get("gpg.format").unwrap_or_else(|| "openpgp".into())
    }

    /// Detached signature over `payload` with the configured key.
    fn sign_payload(&self, payload: &[u8]) -> Result<EnvelopeSignature> {
        let format = self.signing_format();
        let signer = self.identity().map(|(_, e)| e).context("no identity to sign with")?;
        let key = self.config_get("user.signingKey");
        let output = match format.as_str() {
            "ssh" => {
                let key = key.context("user.signingKey must name an SSH key to sign attestations")?;
                let program = self.config_get("gpg.ssh.program").unwrap_or_else(|| "ssh-keygen".into());
                run_with_stdin(
                    std::process::Command::new(program).args(["-Y", "sign", "-n", SSH_NAMESPACE, "-f", &key]),
                    payload,
                )?
            }
            "openpgp" => {
                let program = self.config_get("gpg.program").unwrap_or_else(|| "gpg".into());
                let mut cmd = std::process::Command::new(program);
                cmd.args(["--detach-sign", "--armor"]);
                if let Some(k) = &key {
                    cmd.args(["-u", k]);
                }
                run_with_stdin(&mut cmd, payload)?
            }
            other => bail!("signing attestations with gpg.format={other} is not supported"),
        };
        Ok(EnvelopeSignature { format, signer, value: output })
    }

    /// Check an envelope's signature with Git's verification configuration.
    pub fn check_attestation(&self, envelope: &Envelope) -> Result<SignatureCheck> {
        let Some(sig) = &envelope.signature else { return Ok(SignatureCheck::Unsigned) };
        let dir = tempfile_dir(self)?;
        let sig_path = dir.join("attestation.sig");
        std::fs::write(&sig_path, &sig.value)?;
        let payload = envelope.payload();
        let result = match sig.format.as_str() {
            "ssh" => {
                let allowed = self
                    .config_get("gpg.ssh.allowedSignersFile")
                    .context("gpg.ssh.allowedSignersFile must be configured to verify SSH-signed attestations")?;
                let program = self.config_get("gpg.ssh.program").unwrap_or_else(|| "ssh-keygen".into());
                run_with_stdin(
                    std::process::Command::new(program)
                        .args(["-Y", "verify", "-f", &allowed, "-I", &sig.signer, "-n", SSH_NAMESPACE, "-s"])
                        .arg(&sig_path),
                    &payload,
                )
            }
            "openpgp" => {
                let program = self.config_get("gpg.program").unwrap_or_else(|| "gpg".into());
                run_with_stdin(std::process::Command::new(program).args(["--verify"]).arg(&sig_path).arg("-"), &payload)
            }
            other => Err(anyhow!("unsupported signature format {other}")),
        };
        let _ = std::fs::remove_dir_all(&dir);
        Ok(match result {
            Ok(_) => SignatureCheck::Good,
            Err(e) => SignatureCheck::Bad(e.to_string()),
        })
    }
}

fn tempfile_dir(repo: &Repo) -> Result<std::path::PathBuf> {
    let dir = repo.veyra_dir().join(format!("tmp-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn run_with_stdin(cmd: &mut std::process::Command, input: &[u8]) -> Result<String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("could not run {:?}: {e}", cmd.get_program()))?;
    child.stdin.take().expect("piped").write_all(input)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
