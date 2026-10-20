# Contributing to Veyra

## The one rule

> No feature may compromise repository integrity, deterministic core operations, or a developer's
> ability to recover their work. (DESIGN.md §1)

In practice, that means:

- Anything Veyra writes must pass `git fsck --strict` and leave `git status` consistent.
- Any command that can discard worktree content must run inside an `OpLog` transaction with
  `touches_worktree = true`, so it is checkpointed first and can be undone.
- Never "fix" a conflict by overwriting user data. Refuse, explain, and offer `--force`. Forced
  actions are journaled too.

## Clean-room policy (important)

Git is licensed GPL-2.0, and Veyra is MIT OR Apache-2.0.

| You may | You may not |
|---|---|
| Read Git's documentation, including `Documentation/technical/*` format and protocol specs | Translate, port or paraphrase Git's C source code |
| Observe Git's behaviour and write tests that pin it down | Copy code from GPL projects (including mergiraf, which is GPL-3.0) |
| Read, depend on and contribute to gitoxide (MIT/Apache-2.0) | |
| Reuse code from MIT/Apache projects (difftastic, Jujutsu) with attribution | |

If you have studied a specific Git C function closely, write a **behaviour test** describing what
it does and let someone else write the implementation.

## Workflow

```bash
cargo test --workspace
cargo clippy --workspace --all-targets   # keep it warning-free
```

- Pure logic (secret rules, intent guards, flat-tree diffs, three-way restore) gets unit tests in
  its own crate.
- Anything touching a repository gets an end-to-end test in `crates/veyra-cli/tests/cli.rs` that
  checks the result **with real Git**.
- Test secrets are assembled at runtime (`format!("gh{}", …)`) so Veyra's own pre-commit scan never
  flags the test suite.
- Keep Git's command spellings canonical. New names are aliases (DESIGN.md A5).
- Every user-visible command change updates [docs/COMMANDS.md](docs/COMMANDS.md).

## Commit messages

Conventional Commits (`feat(ops): …`, `fix(core): …`). If an AI assistant helped, declare it with
`veyra commit --assisted-by "<tool>"`.
