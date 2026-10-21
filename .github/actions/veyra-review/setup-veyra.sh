#!/usr/bin/env bash
# Put a veyra binary on PATH for the rest of the job (the action's pluggable install step).
#
#   setup-veyra.sh plan     print cache-key= and cache-path= for actions/cache (empty: don't cache)
#   setup-veyra.sh install  install veyra and add it to GITHUB_PATH
#
# Environment (from the action's inputs):
#   VEYRA_INSTALL      source | release | none
#   VEYRA_VERSION      source: a tag, branch or commit of VEYRA_REPO; release: a release tag.
#                      Empty: VEYRA_ACTION_REF (the action's own ref), else main (source) or the
#                      latest release (release; an action ref that isn't a vX tag is ignored).
#   VEYRA_SOURCE_PATH  source: build this checkout of the Veyra workspace instead of fetching one.
#   VEYRA_REPO         default https://github.com/RMV-Coder/veyra
set -euo pipefail

mode=${VEYRA_INSTALL:-source}
version=${VEYRA_VERSION:-}
if [ -z "$version" ]; then
  action_ref=${VEYRA_ACTION_REF:-}
  if [ "$mode" = source ] || [[ $action_ref =~ ^v[0-9] ]]; then
    version=$action_ref
  fi
fi
source_path=${VEYRA_SOURCE_PATH:-}
repo=${VEYRA_REPO:-https://github.com/RMV-Coder/veyra}
action_root=$(cd "$(dirname "$0")/../../.." && pwd)

# For source installs from the repository: how to name the ref to cargo, and a stable id for it.
resolve_ref() {
  ref=${version:-main}
  if [[ $ref =~ ^[0-9a-f]{40}$ ]]; then
    ref_kind=rev ref_id=$ref
    return
  fi
  local tag head
  tag=$(git ls-remote "$repo" "refs/tags/$ref" | cut -f1)
  head=$(git ls-remote "$repo" "refs/heads/$ref" | cut -f1)
  if [ -n "$tag" ]; then
    ref_kind=tag ref_id=$tag
  elif [ -n "$head" ]; then
    ref_kind=branch ref_id=$head
  else
    echo "::error::$ref is not a tag, branch or full commit id of $repo" >&2
    exit 1
  fi
}

install_dir() {
  echo "${RUNNER_TOOL_CACHE:-${RUNNER_TEMP:-/tmp}}/veyra/$ref_id"
}

case "${1:-}" in
  plan)
    if [ "$mode" = source ] && [ -z "$source_path" ]; then
      resolve_ref
      echo "cache-key=veyra-${RUNNER_OS:-os}-${RUNNER_ARCH:-arch}-$ref_id"
      echo "cache-path=$(install_dir)"
    else
      echo "cache-key="
      echo "cache-path="
    fi
    ;;
  install)
    case "$mode" in
      none)
        command -v veyra >/dev/null || { echo "::error::install: none, but veyra is not on PATH" >&2; exit 1; }
        ;;
      source)
        if [ -n "$source_path" ]; then
          echo "Building veyra from $source_path"
          cargo build --release --locked -p veyra-cli --manifest-path "$source_path/Cargo.toml"
          (cd "$source_path/target/release" && pwd) >> "$GITHUB_PATH"
        else
          resolve_ref
          dir=$(install_dir)
          if [ -x "$dir/bin/veyra" ] || [ -x "$dir/bin/veyra.exe" ]; then
            echo "Using cached veyra ($ref_kind $ref, $ref_id)"
          else
            echo "Building veyra from $repo ($ref_kind $ref)"
            cargo install --locked --git "$repo" "--$ref_kind" "$ref" --root "$dir" veyra-cli
          fi
          echo "$dir/bin" >> "$GITHUB_PATH"
        fi
        ;;
      release)
        dir="${RUNNER_TEMP:-/tmp}/veyra-bin"
        if [ "${RUNNER_OS:-}" = Windows ]; then
          VEYRA_VERSION=$version VEYRA_INSTALL_DIR=$dir pwsh -NoProfile -File "$action_root/install/install.ps1"
        else
          VEYRA_VERSION=$version VEYRA_INSTALL_DIR=$dir sh "$action_root/install/install.sh"
        fi
        echo "$dir" >> "$GITHUB_PATH"
        ;;
      *)
        echo "::error::install must be source, release or none (got '$mode')" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "usage: setup-veyra.sh plan|install" >&2
    exit 2
    ;;
esac
