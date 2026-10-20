//! `veyra explain` — what a git/veyra command does, how risky it is, and how to recover.

use anyhow::Result;
use serde_json::json;

use crate::{Ctx, ui};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Risk {
    Safe,
    Caution,
    Destructive,
}

struct Entry {
    /// First element is the subcommand; the rest are required flags. Each slot lists aliases.
    pattern: &'static [&'static [&'static str]],
    steps: &'static [&'static str],
    risk: Risk,
    risk_note: &'static str,
    recovery: &'static str,
    veyra: &'static str,
}

const KB: &[Entry] = &[
    Entry {
        pattern: &[&["reset"], &["--hard"]],
        steps: &[
            "Moves the current branch to the given commit (default: HEAD).",
            "Replaces the index with that commit's tree.",
            "Overwrites tracked files in the working tree to match it.",
        ],
        risk: Risk::Destructive,
        risk_note: "Uncommitted changes to tracked files are lost. Commits left behind are only reachable through the reflog.",
        recovery: "Committed states: `git reflog` then `git reset --hard <old-id>`. Uncommitted edits: not recoverable in Git.",
        veyra: "`veyra reset --hard` checkpoints the working tree first; `veyra undo` restores files, index and branch.",
    },
    Entry {
        pattern: &[&["reset"], &["--soft"]],
        steps: &[
            "Moves the current branch to the given commit.",
            "Leaves the index and working tree untouched, so the undone commits' changes stay staged.",
        ],
        risk: Risk::Safe,
        risk_note: "No file content is lost.",
        recovery: "`git reset --soft ORIG_HEAD` or use the reflog.",
        veyra: "`veyra undo commit` (or `veyra undo` right after committing) does this — and `veyra redo` re-applies it.",
    },
    Entry {
        pattern: &[&["reset"], &["--mixed"]],
        steps: &[
            "Moves the current branch to the given commit.",
            "Resets the index to that commit; working tree files are kept as unstaged changes.",
        ],
        risk: Risk::Caution,
        risk_note: "No file content is lost, but carefully staged hunks become unstaged.",
        recovery: "Re-stage, or `git reset ORIG_HEAD`.",
        veyra: "`veyra reset <commit>`; `veyra undo` restores the previous staging exactly.",
    },
    Entry {
        pattern: &[&["reset"]],
        steps: &[
            "With a commit: like `reset --mixed` (moves the branch, resets the index).",
            "With paths (`git reset <file>`): unstages those files only.",
        ],
        risk: Risk::Caution,
        risk_note: "Staging is lost; file content is kept.",
        recovery: "Re-run `git add`.",
        veyra: "`veyra unstage <file>` is the clearer spelling; `veyra undo` re-stages.",
    },
    Entry {
        pattern: &[&["revert"]],
        steps: &[
            "Creates a new commit that applies the inverse of the given commit.",
            "History is not rewritten, so it is safe on shared branches.",
        ],
        risk: Risk::Safe,
        risk_note: "Can conflict if later commits touched the same lines.",
        recovery: "Revert the revert, or `git reset --hard HEAD~1` if not pushed.",
        veyra: "Planned (Phase 2). Use `git revert` meanwhile; `veyra undo` can undo it.",
    },
    Entry {
        pattern: &[&["restore"], &["--staged", "-S"]],
        steps: &[
            "Copies the HEAD version of the paths into the index (unstages them).",
            "Working tree files are not touched.",
        ],
        risk: Risk::Safe,
        risk_note: "Only staging changes.",
        recovery: "`git add` the paths again.",
        veyra: "`veyra unstage <paths>`.",
    },
    Entry {
        pattern: &[&["restore"]],
        steps: &[
            "Overwrites working tree files with the version from the index (or --source).",
            "Your uncommitted edits to those files are discarded.",
        ],
        risk: Risk::Destructive,
        risk_note: "Discarded edits were never stored anywhere by Git.",
        recovery: "None in Git, unless an editor or IDE keeps local history.",
        veyra: "`veyra discard <paths>` / `veyra restore` checkpoint first; `veyra undo` brings the edits back.",
    },
    Entry {
        pattern: &[&["checkout"], &["--"]],
        steps: &[
            "`git checkout -- <file>` overwrites the file with the index version (old spelling of `git restore`).",
        ],
        risk: Risk::Destructive,
        risk_note: "Uncommitted edits to the file are lost.",
        recovery: "None in Git.",
        veyra: "`veyra discard <file>` (checkpointed, undoable).",
    },
    Entry {
        pattern: &[&["checkout"], &["-b"]],
        steps: &["Creates a branch at HEAD (or the given start point) and switches to it."],
        risk: Risk::Safe,
        risk_note: "Local changes are carried over.",
        recovery: "`git switch -` and `git branch -d <name>`.",
        veyra: "`veyra switch -c <name>`.",
    },
    Entry {
        pattern: &[&["checkout"]],
        steps: &[
            "With a branch: switches to it, updating index and files.",
            "With a commit: detaches HEAD there.",
            "With paths: overwrites files (see `checkout --`).",
        ],
        risk: Risk::Caution,
        risk_note: "One command, three behaviours — the path form discards edits.",
        recovery: "`git switch -` returns to the previous branch.",
        veyra: "`veyra switch` for branches, `veyra discard` for files. Both are journaled.",
    },
    Entry {
        pattern: &[&["switch"]],
        steps: &[
            "Switches to a branch, updating index and working tree.",
            "Refuses if local changes would be overwritten (unless --discard-changes).",
        ],
        risk: Risk::Safe,
        risk_note: "Safe by default.",
        recovery: "`git switch -`.",
        veyra: "`veyra switch`; even `--force` is checkpointed and undoable.",
    },
    Entry {
        pattern: &[&["clean"]],
        steps: &["Deletes untracked files from the working tree (-d: directories too, -x: ignored files too)."],
        risk: Risk::Destructive,
        risk_note: "Untracked files were never in Git; deleted files are gone.",
        recovery: "None. Always run `git clean -n` (dry run) first.",
        veyra: "Not implemented yet. Veyra checkpoints include untracked files, so `veyra checkpoint` before cleaning makes it recoverable.",
    },
    Entry {
        pattern: &[&["stash"], &["drop", "clear"]],
        steps: &["Deletes stash entries."],
        risk: Risk::Destructive,
        risk_note: "Dropped stashes are only findable as dangling commits until gc.",
        recovery: "`git fsck --unreachable | grep commit`, then inspect candidates.",
        veyra: "Stash is planned (Phase 2). Checkpoints are the safer alternative today.",
    },
    Entry {
        pattern: &[&["stash"], &["pop"]],
        steps: &["Re-applies the latest stash to the working tree and drops it if it applied cleanly."],
        risk: Risk::Caution,
        risk_note: "Conflicts leave the stash in place; a clean pop deletes it.",
        recovery: "The dropped stash's id is printed; `git stash apply <id>`.",
        veyra: "Planned as `veyra unshelve` (Phase 2).",
    },
    Entry {
        pattern: &[&["stash"]],
        steps: &["Saves uncommitted changes to a stash commit and resets the working tree to HEAD."],
        risk: Risk::Safe,
        risk_note: "Untracked files are not stashed unless -u.",
        recovery: "`git stash pop`.",
        veyra: "Planned as `veyra shelve` (Phase 2); `veyra checkpoint` covers the safety use case now.",
    },
    Entry {
        pattern: &[&["push"], &["--force", "-f"]],
        steps: &["Overwrites the remote branch with your local one, even if the remote has commits you don't."],
        risk: Risk::Destructive,
        risk_note: "Teammates' commits on the remote can be silently discarded.",
        recovery: "Someone with the lost commits must push them again; check the remote's reflog/UI.",
        veyra: "Prefer `--force-with-lease`. `veyra push` is planned (Phase 2) and will refuse plain --force by default.",
    },
    Entry {
        pattern: &[&["push"], &["--force-with-lease"]],
        steps: &["Force-pushes only if the remote branch still points where you last saw it."],
        risk: Risk::Caution,
        risk_note: "Safer than --force, but a recent fetch can make the lease stale.",
        recovery: "As with --force.",
        veyra: "Planned (Phase 2).",
    },
    Entry {
        pattern: &[&["push"]],
        steps: &["Uploads local commits and updates the remote branch (fast-forward only)."],
        risk: Risk::Safe,
        risk_note: "Published commits shouldn't be rewritten afterwards.",
        recovery: "`git revert` published mistakes.",
        veyra: "Planned (Phase 2). `git push` works on Veyra repositories meanwhile.",
    },
    Entry {
        pattern: &[&["pull"]],
        steps: &["Runs `git fetch`, then merges (or rebases) the upstream branch into the current one."],
        risk: Risk::Caution,
        risk_note: "Can create unexpected merge commits or conflicts.",
        recovery: "`git merge --abort` or `git reset --hard ORIG_HEAD`.",
        veyra: "`veyra pull [--ff-only]`; `veyra undo` reverts the integration.",
    },
    Entry {
        pattern: &[&["fetch"]],
        steps: &[
            "Downloads objects and updates remote-tracking branches (origin/*). Your branches and files are untouched.",
        ],
        risk: Risk::Safe,
        risk_note: "Read-only for your work.",
        recovery: "Nothing to recover.",
        veyra: "`veyra fetch`.",
    },
    Entry {
        pattern: &[&["rebase"], &["-i", "--interactive"]],
        steps: &["Opens a todo list to reorder, squash, edit or drop commits, then rewrites them."],
        risk: Risk::Destructive,
        risk_note: "Rewrites history; dropped commits disappear from branches.",
        recovery: "`git rebase --abort` during; `git reflog` + `git reset --hard <old>` after.",
        veyra: "Planned (Phase 2). `veyra undo` already reverts a finished `git rebase` (recorded as an external operation).",
    },
    Entry {
        pattern: &[&["rebase"]],
        steps: &["Re-applies your commits on top of another base, creating new commits."],
        risk: Risk::Caution,
        risk_note: "Rewrites history; never rebase commits others have.",
        recovery: "`git rebase --abort`, or the reflog.",
        veyra: "Planned (Phase 2); `veyra undo` reverts a finished git rebase.",
    },
    Entry {
        pattern: &[&["commit"], &["--amend"]],
        steps: &["Replaces the last commit with a new one that includes the staged changes and/or a new message."],
        risk: Risk::Caution,
        risk_note: "Rewrites the last commit; don't amend pushed commits.",
        recovery: "`git reset --soft HEAD@{1}`.",
        veyra: "`veyra commit --amend`; `veyra undo` restores the original commit.",
    },
    Entry {
        pattern: &[&["commit"]],
        steps: &["Records the staged changes as a new commit on the current branch."],
        risk: Risk::Safe,
        risk_note: "Nothing is lost.",
        recovery: "`git reset --soft HEAD~1` to un-commit.",
        veyra: "`veyra commit` (adds secret scanning, intent checks, and --assisted-by / --intent trailers); `veyra undo` un-commits.",
    },
    Entry {
        pattern: &[&["cherry-pick"]],
        steps: &["Applies the changes of existing commits as new commits on the current branch."],
        risk: Risk::Caution,
        risk_note: "Can conflict; duplicates commits across branches.",
        recovery: "`git cherry-pick --abort`.",
        veyra: "Planned (Phase 2).",
    },
    Entry {
        pattern: &[&["merge"], &["--abort"]],
        steps: &["Stops an in-progress merge and restores the pre-merge state."],
        risk: Risk::Caution,
        risk_note: "Conflict resolutions in progress are discarded.",
        recovery: "Re-run the merge.",
        veyra: "`veyra merge --abort` (journaled — even the abort can be undone).",
    },
    Entry {
        pattern: &[&["merge"]],
        steps: &[
            "Joins another branch into the current one: fast-forward when possible, otherwise a merge commit.",
            "Stops with conflict markers if both sides changed the same lines.",
        ],
        risk: Risk::Safe,
        risk_note: "Conflicts need manual resolution.",
        recovery: "`git merge --abort`, or `git reset --hard ORIG_HEAD` after.",
        veyra: "`veyra merge`; `veyra undo` reverts it.",
    },
    Entry {
        pattern: &[&["branch"], &["-D"]],
        steps: &["Deletes a branch even if its commits aren't merged anywhere."],
        risk: Risk::Destructive,
        risk_note: "Unmerged commits become unreachable (reflog of HEAD may still have them).",
        recovery: "`git reflog`, find the tip, `git branch <name> <id>`.",
        veyra: "`veyra branch -D`; `veyra undo` restores the branch.",
    },
    Entry {
        pattern: &[&["branch"], &["-d", "--delete"]],
        steps: &["Deletes a branch if it is fully merged."],
        risk: Risk::Safe,
        risk_note: "Refuses unmerged branches.",
        recovery: "Recreate it at the printed commit id.",
        veyra: "`veyra branch -d` / `veyra branch delete`; `veyra undo` restores it.",
    },
    Entry {
        pattern: &[&["branch"]],
        steps: &["Lists branches, or creates one at HEAD (`git branch <name>`) without switching to it."],
        risk: Risk::Safe,
        risk_note: "—",
        recovery: "—",
        veyra: "`veyra branch`, plus `veyra branch stale`.",
    },
    Entry {
        pattern: &[&["rm"], &["--cached"]],
        steps: &["Stops tracking the file (stages its deletion) but keeps it on disk."],
        risk: Risk::Safe,
        risk_note: "The file stays in history.",
        recovery: "`git add` it again.",
        veyra: "`veyra rm --cached`.",
    },
    Entry {
        pattern: &[&["rm"]],
        steps: &["Deletes the file from the working tree and stages the deletion."],
        risk: Risk::Caution,
        risk_note: "Refuses files with uncommitted changes unless -f.",
        recovery: "`git restore --staged --worktree <file>` before committing.",
        veyra: "`veyra rm`; `veyra undo` restores the file.",
    },
    Entry {
        pattern: &[&["gc"]],
        steps: &["Packs objects and prunes unreachable ones older than the grace period."],
        risk: Risk::Caution,
        risk_note: "After pruning, dangling commits/blobs are gone for good.",
        recovery: "None for pruned objects.",
        veyra: "Safe with Veyra: refs/veyra/keep protects journal snapshots. `veyra doctor` tells you when gc is worthwhile.",
    },
    Entry {
        pattern: &[&["reflog"]],
        steps: &["Shows where HEAD and branches pointed recently — Git's safety net for committed states."],
        risk: Risk::Safe,
        risk_note: "Read-only.",
        recovery: "—",
        veyra: "`veyra op log` records every operation, including staging and uncommitted files.",
    },
    Entry {
        pattern: &[&["filter-branch"]],
        steps: &["Rewrites every commit in history with a filter."],
        risk: Risk::Destructive,
        risk_note: "Rewrites all commit ids; deprecated in favour of git-filter-repo.",
        recovery: "refs/original/* backups, if kept.",
        veyra: "Planned as `veyra history rewrite` (late phase).",
    },
    Entry {
        pattern: &[&["add"], &["-p", "--patch"]],
        steps: &["Interactively chooses hunks to stage."],
        risk: Risk::Safe,
        risk_note: "—",
        recovery: "`git restore --staged`.",
        veyra: "Planned (Phase 2).",
    },
    Entry {
        pattern: &[&["add"]],
        steps: &["Stages file contents for the next commit (including deletions)."],
        risk: Risk::Safe,
        risk_note: "—",
        recovery: "`git restore --staged <file>`.",
        veyra: "`veyra add`; `veyra undo` unstages.",
    },
    Entry {
        pattern: &[&["tag"], &["-d", "--delete"]],
        steps: &["Deletes a local tag."],
        risk: Risk::Caution,
        risk_note: "Remote tags are unaffected.",
        recovery: "Recreate it at the printed id.",
        veyra: "`veyra tag -d`; `veyra undo` restores it.",
    },
    Entry {
        pattern: &[&["undo"]],
        steps: &[
            "Reverses the most recent operation in Veyra's journal — any Veyra command, or a change made with plain git.",
            "Only touches what that operation changed, and refuses to overwrite later edits unless --force.",
        ],
        risk: Risk::Safe,
        risk_note: "The current state is checkpointed first, so undo is itself undoable.",
        recovery: "`veyra redo`.",
        veyra: "`veyra undo [--dry-run]`.",
    },
    Entry {
        pattern: &[&["checkpoint"]],
        steps: &[
            "Snapshots the working tree (tracked and untracked, non-ignored files) without committing or touching the index.",
        ],
        risk: Risk::Safe,
        risk_note: "Stored locally, never pushed.",
        recovery: "`veyra checkpoint restore <id>`.",
        veyra: "`veyra checkpoint`, `veyra checkpoint list`, `veyra checkpoint watch`.",
    },
];

fn find(args: &[String]) -> Option<(&'static Entry, Vec<String>)> {
    let mut words: Vec<String> = args.iter().flat_map(|a| a.split_whitespace().map(str::to_owned)).collect();
    if matches!(words.first().map(String::as_str), Some("git" | "veyra")) {
        words.remove(0);
    }
    let first = words.first()?.clone();
    let best = KB
        .iter()
        .filter(|e| e.pattern[0].contains(&first.as_str()))
        .filter(|e| e.pattern[1..].iter().all(|alts| words.iter().any(|w| alts.contains(&w.as_str()))))
        .max_by_key(|e| e.pattern.len())?;
    Some((best, words))
}

pub fn run(ctx: &Ctx, args: &[String]) -> Result<()> {
    let Some((entry, words)) = find(args) else {
        anstream::println!("No explanation for `{}` yet.", args.join(" "));
        anstream::println!("Known commands: {}", {
            let mut names: Vec<&str> = KB.iter().map(|e| e.pattern[0][0]).collect();
            names.dedup();
            names.join(", ")
        });
        return Ok(());
    };
    let (risk, risk_label) = match entry.risk {
        Risk::Safe => ("safe", ui::green("Safe")),
        Risk::Caution => ("caution", ui::yellow("Use with care")),
        Risk::Destructive => ("destructive", ui::red("Destructive")),
    };
    if ctx.json {
        ui::json(
            &json!({"command": words.join(" "), "steps": entry.steps, "risk": risk, "risk_note": entry.risk_note, "recovery": entry.recovery, "veyra": entry.veyra}),
        );
        return Ok(());
    }
    anstream::println!("{}\n\n  git {}\n", ui::bold("COMMAND EXPLANATION"), words.join(" "));
    anstream::println!("{}", ui::bold("This command:"));
    for (i, s) in entry.steps.iter().enumerate() {
        anstream::println!("  {}. {s}", i + 1);
    }
    anstream::println!("\n{} {risk_label} — {}", ui::bold("Risk:"), entry.risk_note);
    anstream::println!("{} {}", ui::bold("Recovery:"), entry.recovery);
    anstream::println!("{} {}", ui::bold("In Veyra:"), entry.veyra);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::find;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn most_specific_entry_wins() {
        assert_eq!(find(&words("git reset --hard HEAD~1")).unwrap().0.pattern, &[&["reset"][..], &["--hard"][..]]);
        assert_eq!(find(&words("reset HEAD~1")).unwrap().0.pattern.len(), 1);
        assert_eq!(find(&words("veyra push -f origin main")).unwrap().0.pattern[1], &["--force", "-f"][..]);
        assert!(find(&words("git frobnicate")).is_none());
    }

    #[test]
    fn quoted_single_argument_works() {
        assert_eq!(find(&["reset --hard".to_owned()]).unwrap().0.pattern[1], &["--hard"][..]);
    }
}
