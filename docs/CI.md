# Veyra in pull requests

Veyra stores everything in plain Git, so GitHub already shows a commit's trailers
(`Assisted-by`, `Intent`, `Veyra-Structural-Merge`) and signatures. What it can't show is what
Veyra *computes*: the impact of a change, analyzer findings, changed symbols. The **Veyra review**
action runs those analyses on every pull request and puts the result where reviewers look: the
job summary and one PR comment, updated on each push instead of adding a new one.

The review contains:

- **Risk** (low / medium / high), its score and the reasons for it (`veyra impact`).
- **Analyzer findings**: severity, `file:line`, the offending line, and advice (`veyra analyze`).
- **Tests to run**, and why each one is suggested.
- **May be affected**: files that import a changed file or mention a changed symbol, by
  confidence, with the evidence for each.
- **Changed symbols**: functions, methods and types added, removed, modified, renamed or with a
  changed signature, with lines (`veyra diff --semantic`).
- **Commits**: signature status of each of the PR's commits (good, unverified key, unsigned, bad),
  their trailers, and the files Veyra merged structurally (`Veyra-Structural-Merge`), which deserve
  a second look.
- **Usually changed together**: files that change with these ones in history but not in this PR.

Everything is deterministic; no AI is involved.

## Usage

```yaml
# .github/workflows/veyra-review.yml
name: Veyra review
on:
  pull_request:

permissions:
  contents: read
  pull-requests: write   # for the PR comment

jobs:
  review:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0     # the merge base and co-change history
      - uses: dtolnay/rust-toolchain@stable   # install: source builds veyra with cargo
      - uses: RMV-Coder/veyra/.github/actions/veyra-review@main
        with:
          fail-on: high      # optional: fail the job on high-severity analyzer findings
```

The action pins Veyra to its own ref: `@main` builds Veyra's `main`, `@v1.2.0` builds v1.2.0.
The first run builds Veyra with cargo (a few minutes); later runs restore the binary from the
Actions cache. Once release binaries are published, `install: release` downloads one instead (the
archive's checksum is verified, and its build provenance when `gh` is available) and the Rust
toolchain step can go.

Veyra's own repository runs it in [`.github/workflows/veyra-review.yml`](../.github/workflows/veyra-review.yml),
with `source-path: .` so each PR is reviewed by the Veyra it builds.

### Use `pull_request`, not `pull_request_target`

`pull_request_target` runs with a write token in the context of the base branch; checking out and
analysing the PR's code there is a well-known way to leak that token. With `pull_request`, PRs
from forks get a read-only token: the action can't comment, says so in a warning, and the review
is still in the job summary.

## Inputs

| Input | Default | |
|---|---|---|
| `fail-on` | _(empty)_ | `high`, `medium` or `low`: fail the job like `veyra analyze --fail-on`. The review and comment are written first. Empty never fails. |
| `comment` | `true` | Post the review as a PR comment, or update the one posted before. |
| `install` | `source` | `source`: `cargo install` from the Veyra repository (needs a Rust toolchain). `release`: download a release binary. `none`: `veyra` is already on `PATH`. |
| `version` | the action's ref | A tag, branch or commit (`source`), or a release tag (`release`; latest when empty). |
| `source-path` | _(empty)_ | With `install: source`, build the Veyra workspace at this path instead. |
| `base` | the PR's base branch | Branch to compare with. |
| `head` | the PR's head commit | Commit to review. `actions/checkout` checks out GitHub's test merge commit for pull requests; the action switches to this commit (detached) when `HEAD` differs, so the merge commit isn't counted as an unsigned PR commit. |
| `pr-number` | the PR's number | Pull request to comment on. |
| `github-token` | `github.token` | Token for the comment. |
| `max-commits` | `250` | Check signatures and trailers of at most this many of the PR's newest commits. |

## Outputs

`risk`, `risk-score`, `findings`, `high-findings`, `tests`, `unsigned-commits`,
`unverified-signatures`, `bad-signatures`, and `summary-file` (the rendered Markdown), for later
steps such as labelling high-risk PRs:

```yaml
      - uses: RMV-Coder/veyra/.github/actions/veyra-review@main
        id: veyra
      - if: steps.veyra.outputs.risk == 'high'
        run: gh pr edit ${{ github.event.pull_request.number }} --add-label "high risk"
        env:
          GH_TOKEN: ${{ github.token }}
```

## How it works

1. **Install** Veyra (`setup-veyra.sh`): from source, a release, or what's on `PATH`.
2. **Find the range.** Fetch the base branch, switch to the PR head, and deepen a shallow clone
   until the merge base is reachable (`fetch-depth: 0` avoids that).
3. **Collect** (`collect.sh`), all with `--json`, against the fork point:
   `veyra impact --branch <fork>`, `veyra analyze --branch <fork>`,
   `veyra diff --semantic <fork> HEAD`, and for each commit in `base..HEAD`, `veyra log -n 1` (for
   its trailers) and `veyra verify commit`. `veyra verify --history` would walk everything
   reachable from `HEAD`, including commits merged in from the base branch, so each PR commit is
   verified on its own. A failing analysis doesn't stop the others; its section shows the error.
4. **Render** (`render.py`, Python 3 standard library): one Markdown review, written to
   `$GITHUB_STEP_SUMMARY`. Paths, commit messages and code excerpts come from the PR, so they
   are escaped: no HTML, no `@mentions`, no table-breaking pipes. Long sections are collapsed or
   dropped to stay under GitHub's 65,536-character comment limit.
5. **Comment.** The review starts with a hidden marker, `<!-- veyra-review -->`. The action
   looks for a comment by a bot that starts with it and edits it; otherwise it posts one. (With a
   personal access token the comment is a user's, not a bot's, so each run would post a new one;
   use `github.token` or a GitHub App token.)
6. **Gate.** With `fail-on`, run `veyra analyze --branch <fork> --fail-on <severity>` last.

### Signatures on the runner

"Good" means the signature verifies *and* the runner trusts the key, using Git's own
configuration (`gpg.ssh.allowedSignersFile` for SSH signatures, the GnuPG keyring for OpenPGP).
A fresh runner trusts nobody, so a sound signature shows as **unverified key** until you add the
keys before the action runs, e.g. a committed `allowed_signers` file:

```yaml
      - run: git config gpg.ssh.allowedSignersFile "$PWD/.github/allowed_signers"
```

**Bad** means anything else the verifier reports: the signature doesn't match the commit (it was
changed after signing, or the signature is malformed), or a problem such as an expired key.

## Running it locally

The scripts work outside Actions, with `bash`, `git`, Python 3 and `veyra` on `PATH`:

```bash
bash .github/actions/veyra-review/collect.sh origin/main /tmp/veyra-review
python3 .github/actions/veyra-review/render.py /tmp/veyra-review > review.md
```

The renderer's tests use real Veyra output in `tests/fixtures/`; with a built `veyra` (in
`target/` or `$VEYRA`) they also run `collect.sh` against a scratch repository:

```bash
python3 -m unittest discover -s .github/actions/veyra-review/tests
```
