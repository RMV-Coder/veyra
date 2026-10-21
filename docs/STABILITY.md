# Stability promise (1.x)

From 1.0, Veyra follows [semantic versioning](https://semver.org/). Within 1.x, the following are
stable. Anything not listed here (internal crate APIs, the exact wording of human-readable output,
colours, hints) may change in any release.

## Command line

* Commands, subcommands, flags and their meaning don't change incompatibly. New flags and commands
  may be added.
* Deprecated flags keep working for the rest of 1.x and print a warning that names the
  replacement.
* Exit codes: `0` success; `1` failure, or a finding that the command reports as a failure
  (secrets, intent violations, `analyze --fail-on`, conflicts); `2` usage errors.
* Agent mode (`VEYRA_AGENT=1`) keeps refusing destructive actions without `--yes`.

## `--json` output

* Fields are never removed, renamed or changed in type in 1.x. New fields may be added, so
  consumers must ignore unknown fields.
* Enumerated string values (`"high"`, `"medium"`, …) may gain new values; consumers should treat
  unknown values conservatively.
* Every `--json` output is a single JSON document on stdout. Diagnostics go to stderr.

## Repository data

* **Git data**: Veyra writes standard Git formats only; see [COMPATIBILITY.md](COMPATIBILITY.md).
* **Operation journal** (`.git/veyra/oplog.jsonl`): one JSON object per line. Records written by
  any 1.x release can be read (and undone) by every later 1.x release. Readers ignore unknown
  fields, so newer records stay readable by older 1.x versions, which simply don't use the new
  information. A torn last line from a crash is ignored and cleaned up on the next write.
* **Committed policy files** (`.veyra/intents/*.toml`, `secrets.toml`, `scopes.toml`,
  `analyzers.toml`): files valid in 1.0 stay valid in 1.x with the same meaning.
* **Trailers** Veyra writes (`Assisted-by`, `Intent`, `Veyra-Structural-Merge`) keep their names
  and meaning.
* **Attestations** stay in-toto v1 Statements in `refs/notes/veyra/attestations`.

## Configuration

`veyra.*` Git config keys and their defaults are stable. A default changes only if the old
behaviour was unsafe, and the release notes say so.

## Not covered

* The Rust APIs of the `veyra-*` library crates. They are versioned with the binary but may change
  in minor releases until they are published as separately versioned crates.
* AI output. Models and prompts improve; what stays stable is that AI features are off by default,
  everything they produce is a proposal, and nothing is changed without approval.
