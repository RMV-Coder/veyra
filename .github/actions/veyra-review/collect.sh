#!/usr/bin/env bash
# Run Veyra's analyses of the change between BASE and HEAD and write their JSON for render.py.
#
#   collect.sh <base-rev> <out-dir>
#
# Environment: VEYRA (the binary, default `veyra`), VEYRA_REVIEW_MAX_COMMITS (default 250),
# VEYRA_REVIEW_HISTORY (commits `impact` mines for co-changes, default 500),
# VEYRA_REVIEW_BASE_LABEL (how the review names the base, default <base-rev>).
#
# Each analysis writes <name>.json and <name>.err. A failing analysis doesn't stop the others:
# render.py reports it in its section. Needs git (for the merge base and the PR's commit list).
set -uo pipefail

base=${1:?usage: collect.sh <base-rev> <out-dir>}
out=${2:?usage: collect.sh <base-rev> <out-dir>}
veyra=${VEYRA:-veyra}
max_commits=${VEYRA_REVIEW_MAX_COMMITS:-250}
history=${VEYRA_REVIEW_HISTORY:-500}
mkdir -p "$out"

fork=$(git merge-base "$base" HEAD) || {
  echo "::error::$base and HEAD have no merge base; fetch more history (actions/checkout fetch-depth: 0)" >&2
  exit 1
}
head=$(git rev-parse HEAD)
total=$(git rev-list --count "$base..HEAD")
{
  echo "base=${VEYRA_REVIEW_BASE_LABEL:-$base}"
  echo "fork=$fork"
  echo "head=$head"
  echo "commits=$total"
  echo "veyra=$("$veyra" --version 2>/dev/null)"
} > "$out/meta.txt"

# The fork point is passed as a commit id, so the analyses match `--branch <base>` exactly without
# depending on how the base branch is named locally.
"$veyra" impact --branch "$fork" --history "$history" --json > "$out/impact.json" 2> "$out/impact.err"
"$veyra" analyze --branch "$fork" --json > "$out/analyze.json" 2> "$out/analyze.err"
"$veyra" diff --semantic "$fork" "$head" --json > "$out/semantic.json" 2> "$out/semantic.err"

# Signatures and trailers of the PR's own commits (base..HEAD, newest `max_commits`). Each commit
# is verified on its own: `verify --history` walks everything reachable from a tip, which would
# include commits merged in from the base branch.
: > "$out/commits.json"
: > "$out/verify.json"
: > "$out/commits.err"
: > "$out/verify.err"
git rev-list --reverse --max-count="$max_commits" "$base..HEAD" | while read -r sha; do
  "$veyra" log -n 1 --json "$sha" >> "$out/commits.json" 2>> "$out/commits.err"
  # Exits non-zero for unsigned and bad signatures; the JSON says which.
  "$veyra" verify commit "$sha" --json >> "$out/verify.json" 2>> "$out/verify.err" || true
done
exit 0
