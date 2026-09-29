#!/bin/sh
# What's installed, and how it compares to this checkout and the latest
# release. Read-only.
#
# POSIX sh on purpose: mise runs project tasks under sh.
set -u

repo=rdlu/rodii-power-menu
dest="$HOME/.local/bin/rodii-power-menu"
build=target/release/rodii-power-menu
src=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
latest=$(curl -fsSL --max-time 5 "https://api.github.com/repos/$repo/releases/latest" 2>/dev/null |
	sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)

row() { printf '%-16s %s\n' "$1" "$2"; }

if [ -x "$dest" ]; then
	installed=$("$dest" --version 2>/dev/null | cut -d' ' -f2)
	[ -n "$installed" ] || installed="(pre-0.1.2 build: no --version)"
	if file "$dest" | grep -q 'static-pie linked'; then
		kind="static release build"
	else
		kind="local build"
	fi
	row installed: "$installed  ($kind, $dest)"
else
	row installed: "no ($dest missing; Mod+Escape does nothing)"
fi
row source: "$src  (this checkout)"
if [ -x "$build" ] && [ -x "$dest" ]; then
	if cmp -s "$build" "$dest"; then
		row "vs. source:" "the installed binary IS this checkout's release build"
	else
		row "vs. source:" "differs from target/release (run: mise run install)"
	fi
fi
row latest: "${latest:-(GitHub not reachable)}"
if [ -x "$dest" ]; then
	printf '%-16s ' config:
	"$dest" validate 2>&1 | head -5
fi
