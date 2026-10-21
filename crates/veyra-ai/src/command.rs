//! The `command` provider: any local model or script.
//!
//! Veyra runs the command through the shell, writes the request as JSON to its stdin —
//! `{"system": "...", "prompt": "...", "schema": {...}?}` — and uses its stdout as the answer.
//! A non-zero exit status is an error (its stderr is shown).

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::{Reply, Request};

fn shell(command: &str) -> Command {
    // Git for Windows ships `sh`; fall back to cmd.exe elsewhere on Windows.
    if cfg!(windows) && which("sh").is_none() {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let exe = dir.join(format!("{program}{}", std::env::consts::EXE_SUFFIX));
        exe.is_file().then_some(exe)
    })
}

pub fn complete(command: &str, request: &Request) -> Result<Reply> {
    let input = serde_json::to_vec(request)?;
    let mut child = shell(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run the AI command `{command}`"))?;
    // Write on a thread so a command that streams output before reading all input can't deadlock.
    let mut stdin = child.stdin.take().context("no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    let _ = writer.join();
    if !output.status.success() {
        bail!(
            "the AI command `{command}` failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8(output.stdout).context("the AI command's output is not UTF-8")?;
    if text.trim().is_empty() {
        bail!("the AI command `{command}` printed nothing");
    }
    Ok(Reply { text, model: None })
}
