#!/bin/sh
# Cut a release: bump the version, build + verify the static tarball, commit
# and push, publish the GitHub release, then download it back and verify it.
# Run through mise, which passes the arguments as usage_* variables:
#
#   mise run release 0.1.2                 # notes = commit subjects since the last tag
#   mise run release 0.1.2 --notes notes.md
#   mise run release 0.1.2 --dry-run       # everything but commit/push/publish
#
# POSIX sh on purpose: mise runs project tasks under sh.
set -eu

bold() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() {
	printf '\033[31mrelease: %s\033[0m\n' "$*" >&2
	exit 1
}

# usage_* are set by mise from the task's usage spec (see mise.toml).
# shellcheck disable=SC2154
: "${usage_version:?run this via: mise run release <version>}"
v=${usage_version#v}
tag="v$v"
dry=${usage_dry_run:-false}
notes=${usage_notes:-}
name="rodii-power-menu-$tag-x86_64-linux"
tarball="dist/$name.tar.gz"
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
url="https://github.com/$repo/releases/download/$tag/$name.tar.gz"

# --- checks (all before anything is changed) ---------------------------------
printf '%s\n' "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "version must look like 1.2.3, got '$v'"
cur=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
newest=$(printf '%s\n%s\n' "$cur" "$v" | sort -V | tail -1)
[ "$v" != "$cur" ] && [ "$newest" = "$v" ] || die "$v is not newer than the current $cur"
[ "$(git branch --show-current)" = main ] || die "not on main"
[ -z "$(git status --porcelain)" ] || die "working tree not clean (commit or stash first)"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists locally"
gh release view "$tag" >/dev/null 2>&1 && die "release $tag already exists on GitHub"
if [ -n "$notes" ]; then [ -s "$notes" ] || die "notes file '$notes' is missing or empty"; fi
rustup target list --installed | grep -qx x86_64-unknown-linux-musl ||
	die "musl target missing: rustup target add x86_64-unknown-linux-musl"

bold "fetching origin (tap the security key if it blinks)"
git fetch -q origin
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
	die "main differs from origin/main (push or pull first)"

bold "clippy"
cargo clippy --release -q -- -D warnings

# --- release notes -------------------------------------------------------------
prev=$(git describe --tags --abbrev=0 2>/dev/null || true)
if [ -z "$notes" ]; then
	notes=$(mktemp)
	trap 'rm -f "$notes"' EXIT
	{
		echo "## Changes${prev:+ since $prev}"
		echo
		git log --format='- %s' "${prev:+$prev..}HEAD" | grep -Ev '^- v[0-9]+\.[0-9]+\.[0-9]+$' || echo "- (no changes listed)"
		echo
		echo "### Install / upgrade"
		echo
		echo "A static (musl) x86_64 Linux binary that runs on any distro. You only need **fuzzel ≥ 1.15**."
		echo
		echo '```sh'
		echo "curl -LO $url"
		echo "sha256sum -c <(curl -sL $url.sha256)"
		echo "tar -xzf $name.tar.gz"
		echo "install -Dm755 $name/rodii-power-menu ~/.local/bin/rodii-power-menu"
		echo '```'
	} >"$notes"
fi

# --- bump + build ----------------------------------------------------------------
# From here on, any failure puts the version files back.
restore() { git checkout -q -- Cargo.toml Cargo.lock README.md; }

bold "bumping $cur → $v"
sed -i "0,/^version = \"$cur\"/s//version = \"$v\"/" Cargo.toml
sed -i "s/^v=v$cur\$/v=$tag/" README.md
cargo build --release -q || { restore; die "build failed"; } # also refreshes Cargo.lock

bold "packaging"
mise run -q package >/dev/null || { restore; die "packaging failed"; }
[ -s "$tarball" ] && [ -s "$tarball.sha256" ] || { restore; die "packaging produced no $tarball"; }
file "dist/$name/rodii-power-menu" | grep -q 'static-pie linked' || { restore; die "binary is not static"; }
"dist/$name/rodii-power-menu" --help >/dev/null || { restore; die "packaged binary doesn't run"; }

if [ "$dry" = true ]; then
	bold "dry run: would commit '$tag', push, and publish $tarball with these notes:"
	echo
	cat "$notes"
	echo
	restore
	bold "dry run done; version files restored (the dist/ build is kept)"
	exit 0
fi

# --- publish ---------------------------------------------------------------------
bold "committing + pushing $tag (tap the security key)"
git commit -q -am "$tag"
git push -q origin main

bold "publishing the GitHub release"
gh release create "$tag" --target main --title "$tag" --notes-file "$notes" "$tarball" "$tarball.sha256"

bold "verifying the published download"
tmp=$(mktemp -d)
(
	cd "$tmp"
	curl -sSLO "$url"
	curl -sSLO "$url.sha256"
	sha256sum -c "$name.tar.gz.sha256"
	tar -xzf "$name.tar.gz"
	"./$name/rodii-power-menu" --help >/dev/null
)
rm -rf "$tmp"
bold "released $tag: https://github.com/$repo/releases/tag/$tag"
