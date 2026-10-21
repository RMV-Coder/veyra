# Veyra

**Version control that understands your code.** Veyra is a Git-compatible version control
system written in Rust. It reads and writes standard Git repositories, so you can use it
alongside `git` in either order, push to GitHub or GitLab as usual, and stop using it at any time.

> **Beta.** Veyra is feature-complete for everyday use and tested against real Git, but so far it
> has only been used by its author, on Windows. Linux and macOS are not tested yet (see
> [Compatibility](COMPATIBILITY.md)). Because it never changes Git's on-disk format, you can always
> continue with plain `git` if something goes wrong.

## What it adds to Git

- **`veyra undo` for everything**, backed by a journal of every operation (including `git`
  commands you ran yourself), and **automatic checkpoints** of your working tree before any
  command touches your files.
- **Secret scanning on commit**, with redacted findings and fingerprint allowlists.
- **Intents**: records of *why* code exists, with guards that warn when a change would remove the
  mechanism they describe.
- **Code intelligence** for Rust, TypeScript/JavaScript, Python, Go, Java, C, C++, C#, Ruby, PHP,
  Kotlin, Swift, Scala, Bash, Lua and Elixir: semantic diffs, structural merges of edits to
  different functions, impact analysis with evidence, and risk analyzers for migrations and
  infrastructure that you can extend with your own rules and [plugins](PLUGINS.md).
- **Optional AI**, off by default: commit messages, splitting a change into commits (by file or
  by hunk), assisted merges and reviews, every one a proposal you approve.
- **`veyra serve`**, a local, read-only viewer for history, signatures, intents, the undo journal
  and checkpoints.

## Quick start

```bash
cargo install --path crates/veyra-cli    # or a release binary, see the README

veyra init my-project && cd my-project
echo "# Hello" > README.md
veyra add .
veyra commit -m "feat: initial commit"

veyra checkpoint -m "before refactor"   # snapshot without committing
veyra op log                            # everything that happened
veyra undo                              # reverse the last operation
veyra doctor --full                     # health report, with every object verified
```

Command names are Git's (`veyra add`, `veyra commit -m`, `veyra switch -c`, …).

## Where to go next

- [Commands](COMMANDS.md): every command, mapped from Git, with its status.
- [Compatibility](COMPATIBILITY.md): what is shared with Git, what is tested, what isn't supported.
- [Veyra in CI](CI.md): the pull-request review action.
- [Analyzer plugins](PLUGINS.md): adding your own checks to `veyra analyze`.
- [Design](DESIGN.md), [Vision](VISION.md), [Stability](STABILITY.md) and [Roadmap](ROADMAP.md).
