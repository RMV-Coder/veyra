//! Launching the user's editor the way Git does: through the shell, as
//! `sh -c '<editor> "$@"' <editor> <file>`, so values like `code --wait` or
//! `"C:/Program Files/Notepad++/notepad++.exe" -multiInst` work unchanged.

use std::path::Path;

use anyhow::{Context, Result, bail};
use veyra_core::Repo;

/// The commit-message editor: `GIT_EDITOR`, `core.editor`, `VISUAL`, `EDITOR`, then a default.
pub fn commit_editor(repo: &Repo) -> String {
    std::env::var("GIT_EDITOR")
        .ok()
        .or_else(|| repo.config_get("core.editor"))
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| if cfg!(windows) { "notepad".into() } else { "vi".into() })
}

/// The todo-list editor for `rebase -i`: `GIT_SEQUENCE_EDITOR`, `sequence.editor`, then the commit editor.
pub fn sequence_editor(repo: &Repo) -> String {
    std::env::var("GIT_SEQUENCE_EDITOR")
        .ok()
        .or_else(|| repo.config_get("sequence.editor"))
        .unwrap_or_else(|| commit_editor(repo))
}

/// Open `file` in `editor` and wait for it to exit.
pub fn run(editor: &str, file: &Path) -> Result<()> {
    if editor == ":" {
        return Ok(()); // git's convention for "don't edit"
    }
    let mut command = veyra_core::gix::path::env::shell_command();
    command.arg("-c").arg(format!("{editor} \"$@\"")).arg(editor).arg(file);
    let status = command.status().with_context(|| {
        format!("could not start the editor '{editor}'\n  hint: set core.editor, or pass the message with -m")
    })?;
    if !status.success() {
        bail!("the editor '{editor}' exited with an error");
    }
    Ok(())
}

/// Write `template` plus a comment footer to `file`, let the user edit it, and return the text.
pub fn edit_text(editor: &str, file: &Path, template: &str, footer: &str) -> Result<String> {
    let mut text = String::from(template);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(footer);
    std::fs::write(file, &text)?;
    run(editor, file)?;
    Ok(std::fs::read_to_string(file)?)
}
