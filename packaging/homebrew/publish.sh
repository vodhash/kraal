#!/bin/sh
# Put kraal VERSION's formula in the Homebrew tap TAP (owner/homebrew-tap), over ssh with the key
# git uses (GIT_SSH_COMMAND). SUMS is the release's SHA256SUMS. DRY_RUN=1 prints the formula and
# stops before reaching the tap.
# Usage: packaging/homebrew/publish.sh VERSION SUMS TAP
set -eu
version=$1 sums=$2 tap=$3
here=$(cd "$(dirname "$0")" && pwd)
formula=$(mktemp)
"$here/../fill.sh" "$here/kraal.rb.in" "$version" "$sums" > "$formula"
if [ "${DRY_RUN:-}" = 1 ]; then
    cat "$formula"
    exit 0
fi
repo=$(mktemp -d)
git clone -q --depth 1 "git@github.com:$tap.git" "$repo"
mkdir -p "$repo/Formula"
# A late run for an older release (a job run again) never takes the tap back.
current=$(sed -n 's/^  version "\(.*\)"$/\1/p' "$repo/Formula/kraal.rb" 2>/dev/null || true)
if [ -n "$current" ] && [ "$(printf '%s\n%s\n' "$current" "$version" | sort -V | tail -1)" != "$version" ]; then
    echo "$tap has kraal $current, newer than $version: left as it is"
    exit 0
fi
cp "$formula" "$repo/Formula/kraal.rb"
cd "$repo"
git add Formula/kraal.rb
if git diff --cached --quiet; then
    echo "$tap has kraal $version already"
    exit 0
fi
git commit -q -m "kraal $version"
git push -q origin HEAD
echo "$tap: kraal $version"
