#!/bin/sh
# Put kraal-bin VERSION on the AUR, over ssh with the key git uses (GIT_SSH_COMMAND): its
# PKGBUILD for the release (SUMS is its SHA256SUMS), built by makepkg first so sources and sums
# are checked, and its .SRCINFO. Runs as a user makepkg accepts, not root. MAINTAINER, when set,
# heads the PKGBUILD. DRY_RUN=1 stops once it is built, before reaching the AUR.
# Usage: packaging/aur/publish.sh VERSION SUMS
set -eu
version=$1 sums=$2
here=$(cd "$(dirname "$0")" && pwd)
build=$(mktemp -d)
{
    if [ -n "${MAINTAINER:-}" ]; then echo "# Maintainer: $MAINTAINER"; fi
    "$here/../fill.sh" "$here/PKGBUILD.in" "$version" "$sums"
} > "$build/PKGBUILD"
(cd "$build" && makepkg -f --noconfirm >&2 && makepkg --printsrcinfo > .SRCINFO)
if [ "${DRY_RUN:-}" = 1 ]; then
    echo "$build"
    exit 0
fi
repo=$(mktemp -d)
git clone -q ssh://aur@aur.archlinux.org/kraal-bin.git "$repo"
# A late run for an older release (a job run again) never takes the AUR back.
current=$(sed -n 's/^pkgver=//p' "$repo/PKGBUILD" 2>/dev/null || true)
if [ -n "$current" ] && [ "$(printf '%s\n%s\n' "$current" "$version" | sort -V | tail -1)" != "$version" ]; then
    echo "the AUR has kraal-bin $current, newer than $version: left as it is"
    exit 0
fi
cp "$build/PKGBUILD" "$build/.SRCINFO" "$repo/"
cd "$repo"
git add PKGBUILD .SRCINFO
if git diff --cached --quiet; then
    echo "the AUR has kraal-bin $version already"
    exit 0
fi
git commit -q -m "kraal-bin $version"
git push -q origin HEAD:master
echo "AUR: kraal-bin $version"
