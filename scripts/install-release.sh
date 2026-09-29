#!/bin/sh
# Install a prebuilt release (static musl binary) into ~/.local/bin, no Rust
# needed. Verifies the sha256, then validates the config.
#
#   mise run install-release           # latest
#   mise run install-release 0.1.1     # a specific version
#
# POSIX sh on purpose: mise runs project tasks under sh.
set -eu

bold() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() {
	printf '\033[31minstall-release: %s\033[0m\n' "$*" >&2
	exit 1
}

repo=rdlu/rodii-power-menu
dest="$HOME/.local/bin/rodii-power-menu"

# usage_version is set by mise from the task's usage spec (empty = latest).
# shellcheck disable=SC2154
v=${usage_version:-}
if [ -z "$v" ]; then
	tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
		sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
	[ -n "$tag" ] || die "couldn't find the latest release of $repo"
else
	tag="v${v#v}"
fi

name="rodii-power-menu-$tag-x86_64-linux"
url="https://github.com/$repo/releases/download/$tag/$name.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

bold "downloading $tag"
cd "$tmp"
curl -fsSLO "$url" || die "no such release asset: $url"
curl -fsSLO "$url.sha256" || die "missing checksum: $url.sha256"
sha256sum -c --quiet "$name.tar.gz.sha256" || die "checksum mismatch"
tar -xzf "$name.tar.gz"

bold "installing to $dest"
mkdir -p "$(dirname "$dest")"
rm -f "$dest" # never write through a stale symlink at the target
install -m755 "$name/rodii-power-menu" "$dest"
"$dest" --version 2>/dev/null || echo "rodii-power-menu $tag (no --version before v0.1.2)"
"$dest" validate || echo "(install done, but fix the config above before using the menu)"
