//! `veyra` — version control that understands your code.

mod cli;
mod cmd;
mod editor;
mod explain;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Parser;
use veyra_core::Repo;

/// Shared state for every command.
pub struct Ctx {
    pub cwd: PathBuf,
    pub json: bool,
    pub yes: bool,
    /// `VEYRA_AGENT=1`: an AI agent is driving; destructive commands need `--yes`.
    pub agent: bool,
    /// The command line, recorded in the operation log.
    pub argv: String,
}

impl Ctx {
    pub fn repo(&self) -> Result<Repo> {
        Repo::discover(&self.cwd)
    }

    /// In agent mode, refuse destructive actions unless `--yes` was given.
    pub fn confirm_destructive(&self, what: &str) -> Result<()> {
        if self.agent && !self.yes {
            bail!(
                "agent mode (VEYRA_AGENT=1): `{what}` can discard work; re-run with --yes to confirm\n  (it would be checkpointed first, so `veyra undo` could still recover it)"
            );
        }
        Ok(())
    }
}

/// An error that has already been reported; just exit with a failure status.
#[derive(Debug)]
pub struct Reported;

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("(reported)")
    }
}

impl std::error::Error for Reported {}

fn main() -> ExitCode {
    // clap's derived parser for a CLI this size, plus deep gitoxide call chains, can exceed
    // Windows' 1 MiB main-thread stack in debug builds. Run on a thread with a roomier stack.
    std::thread::Builder::new()
        .name("veyra".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(real_main)
        .expect("failed to start the main thread")
        .join()
        .unwrap_or(ExitCode::FAILURE)
}

fn real_main() -> ExitCode {
    let cli = cli::Cli::parse();
    let cwd = match &cli.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let argv = std::iter::once("veyra".to_owned())
        .chain(std::env::args().skip(1).map(|a| if a.contains(' ') { format!("{a:?}") } else { a }))
        .collect::<Vec<_>>()
        .join(" ");
    let ctx = Ctx {
        cwd,
        json: cli.json,
        yes: cli.yes,
        agent: std::env::var("VEYRA_AGENT").is_ok_and(|v| !v.is_empty() && v != "0"),
        argv,
    };
    match cmd::run(&ctx, cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.downcast_ref::<Reported>().is_some() => ExitCode::FAILURE,
        Err(err) => {
            let mut msg = format!("{err}");
            // Show the cause chain for errors that come from gitoxide or the OS.
            for cause in err.chain().skip(1) {
                let c = cause.to_string();
                if !msg.contains(&c) {
                    msg.push_str(&format!("\n  cause: {c}"));
                }
            }
            anstream::eprintln!("{} {msg}", ui::red("error:"));
            ExitCode::FAILURE
        }
    }
}
