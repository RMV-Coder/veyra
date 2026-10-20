# Veyra Vision

This document records the brainstorm that started Veyra: a conversation about how Git could be
improved, followed by a proposal for a Rust-based, Git-compatible successor. It summarises that
proposal and gives Veyra's verdict on each idea. The engineering decisions are in
[DESIGN.md](DESIGN.md), and the schedule is in [ROADMAP.md](ROADMAP.md).

> **Git is excellent at remembering what changed. The next generation of version control should
> also help developers understand why it changed, what it affects, and how to safely recover
> when something goes wrong.**

## The name

**Veyra** is short, easy to say and type, and doesn't tie the project to "git" or to version
control alone. The CLI is `veyra`. As of 2026-10-20 the crate name was free on crates.io. Before
any public launch, still check trademarks and GitHub organisation availability.

Tagline: *Veyra — version control that understands your code.*

## The thirteen ideas, and what Veyra does with them

| # | Idea from the brainstorm | Verdict | Where it lives |
|---|---|---|---|
| 1 | **Semantic diff**: understand renames, moved functions and behaviour-preserving changes, never claiming equivalence without evidence | Keep, built on Tree-sitter and difftastic-style structural diffing | Phase 4 |
| 2 | **Intelligent merge**: syntax-aware conflict resolution with Conservative / Assisted / Experimental modes, explained and never silently committed | Keep, with Conservative as the default. Phase 1 already does Git-equivalent merges with conflict markers. | Phase 1 (merge), Phase 4 (structural) |
| 3 | **First-class undo and recovery**: one `undo`, an operation journal, safeguards for pushed work | **Keep and build first.** One vocabulary: `undo`, `redo`, `op log/show/restore`. | ✅ Phase 1 |
| 4 | **Automatic checkpoints** without commits: local, never pushed, retention-managed, secrets excluded | **Keep and build first.** Snapshots on every mutating command, plus opt-in snapshots on every command and a `watch` mode. No daemon. | ✅ Phase 1 |
| 5 | **AI-assisted VCS**: `commit --smart`, `commit --split`, AI provenance | Adjusted. AI is optional and never decides. Provenance uses declared trailers (`--assisted-by`), not guesses. | Provenance ✅ Phase 1. Smart/split in Phase 5. |
| 6 | **Better branch management**: graph, stale, cleanup, compare, explain | Keep | `branch stale` ✅. Graph, cleanup and compare in Phase 2. |
| 7 | **Repository health** (`doctor`) and pre-commit secret scanning | Adjusted: per-check PASS/WARN/FAIL with evidence instead of a 0–100 score | ✅ Phase 1 |
| 8 | **Smarter large-repo support**: `clone --scope`, adaptive fetching | Adjusted: scopes are declared in `.veyra/scopes.toml` and map to partial clone plus sparse cone, not inferred dependencies | Phase 3 |
| 9 | **Change impact analysis**: affected modules, recommended tests, risk, all advisory | Keep, deterministic first (import graphs, test mappings), AI optional | Phase 4 |
| 10 | **Database and infrastructure change analysis** (`DROP COLUMN`, deleted cloud resources) | Keep, as provider plugins. It never replaces migration testing or `terraform plan`. | Phase 5 |
| 11 | **Verifiable history**: signatures, attestations, provenance | Keep, on existing standards (SSH/GPG signing, in-toto/SLSA attestations) | Phase 3 |
| 12 | **A more intuitive CLI**: `undo commit`, `discard`, `shelve`, `explain` | Adjusted: Git spellings stay canonical, and new names are aliases | `explain`, `discard`, `unstage`, `undo commit` ✅. `shelve` in Phase 2. |
| 13 | **Intent-based version control**: structured *why*, linked to commits, with violation warnings | Adjusted: intents are reviewed files with **deterministic guards**, linked through `Intent:` trailers | ✅ Phase 1 |

### The brainstorm's priority ranking, and Veyra's

The brainstorm ranked Unified Undo first, then Checkpoints, Semantic Diff, Intelligent Merge,
Impact Analysis, Health, CLI, AI, Branches, Large Repos, Verified History and Infra Analysis.
Veyra keeps the first two at the top and moves **Git compatibility** above everything else. A
safety net that corrupts repositories is worse than no safety net. See DESIGN.md §2 for the full
list of adjustments.

## Principles kept from the brainstorm

1. **Distributed and offline-first.** Every clone is a full repository, and nothing needs a server.
2. **Content-addressed storage.** It stays Git's object model.
3. **Deterministic core.** No core operation needs AI, a network or an API key.
4. **Explicit developer control.** Veyra suggests and explains. It never silently decides.
5. **Compatibility.** Existing repositories, GitHub, GitLab and every Git tool keep working.
6. **AI as an optional layer.** Core first, then deterministic intelligence (Tree-sitter, static
   analysis), then optional local or remote models.

## The architecture sketch, as adopted

The brainstorm proposed a layered system: a developer interface, an intelligent VCS layer
(semantic diff, intent, merge, impact, recovery, security), a Git compatibility layer, and a
standard `.git/` underneath. Veyra adopts this, with gitoxide as the compatibility layer and far
fewer crates at the start. See DESIGN.md §3.

## Two command styles?

The brainstorm suggested a second, "modern" command set (`stage`, `save`, `publish`). Veyra keeps
only uncontroversial aliases (`stage`, `remove`, `move`, `discard`, `unstage`). As the brainstorm
itself admitted, `save` and `publish` blur concepts (committing is not publishing), so they were
dropped. See [COMMANDS.md](COMMANDS.md).
