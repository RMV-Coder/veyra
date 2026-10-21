#!/bin/sh
# Install the latest (or a given) Veyra release on Linux or macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/RMV-Coder/veyra/main/install/install.sh | sh
#   VEYRA_VERSION=v1.0.0 VEYRA_INSTALL_DIR=~/.local/bin sh install.sh
#
# The archive's SHA-256 is checked against the release's SHA256SUMS before anything is installed.
# With the GitHub CLI available, the build provenance attestation is verified too.
set -eu

repo="RMV-Coder/veyra"
version="${VEYRA_VERSION:-}"
dir="${VEYRA_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Linux) os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *) echo "unsupported OS: $(uname -s) (on Windows use install.ps1)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  arm64 | aarch64) arch="aarch64" ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
target="$arch-$os"

if [ -z "$version" ]; then
  version=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$version" ] || { echo "could not determine the latest release" >&2; exit 1; }
fi

name="veyra-$version-$target"
base="https://github.com/$repo/releases/download/$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading Veyra $version for $target…"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

expected=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || { echo "checksum mismatch; not installing" >&2; exit 1; }

if command -v gh >/dev/null 2>&1; then
  gh attestation verify "$tmp/$name.tar.gz" --repo "$repo" >/dev/null && echo "Provenance verified."
fi

tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 0755 "$tmp/$name/veyra" "$dir/veyra"
echo "Installed $dir/veyra"
case ":$PATH:" in *":$dir:"*) ;; *) echo "Add $dir to your PATH." ;; esac
"$dir/veyra" --version
