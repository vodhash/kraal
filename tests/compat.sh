#!/bin/sh
# The cross-version test (tests/compat.rs): this kraal with sessions that other releases started,
# refusing incompatible protocols and pre-0.4 state, saving and stopping old servers,
# and restoring and restarting 0.4.x and 0.5.x sessions, or testing the releases given:
#
#   tests/compat.sh                    # 0.2.0, 0.3.0, 0.4.0, 0.4.1 and 0.5.3
#   tests/compat.sh v0.2.0             # one release
#   tests/compat.sh --build v0.2.0     # built from its tag instead of downloaded
#   tests/compat.sh --list             # which releases it would test
#
# A release is downloaded from its GitHub release and checked against the release's SHA256SUMS
# before it runs, or built from its tag (`git fetch --tags` first) with --build. Binaries are
# kept in $KRAAL_COMPAT_DIR (target/compat by default) for the next run. Arguments after `--` go
# to the test binary (`tests/compat.sh -- --nocapture`).
set -eu

# Released clients, servers and saved state formats (docs/compatibility.md).
RELEASES="v0.2.0 v0.3.0 v0.4.0 v0.4.1 v0.5.3"

repo=vodhash/kraal
root=$(cd "$(dirname "$0")/.." && pwd)
dir=${KRAAL_COMPAT_DIR:-$root/target/compat}

say() { printf '%s\n' "$*" >&2; }
fail() {
    say "compat: $*"
    exit 1
}

build=0
list=0
tags=""
while [ $# -gt 0 ]; do
    case "$1" in
        --build) build=1 ;;
        --list) list=1 ;;
        --) shift; break ;;
        -h | --help)
            sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        v*) tags="$tags $1" ;;
        *) fail "unknown argument $1 (a release tag such as v0.2.0, --build, --list, or -- and test arguments)" ;;
    esac
    shift
done

[ -n "$tags" ] || tags=$RELEASES
if [ "$list" = 1 ]; then
    # Tags given as arguments each came after a space.
    echo "${tags# }"
    exit 0
fi

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

# The release archive for this machine, as install.sh picks it.
asset() {
    case "$(uname -s)" in
        Linux) os=unknown-linux-musl ;;
        Darwin) os=apple-darwin ;;
        *) fail "no release for $(uname -s)" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) fail "no release for $(uname -m)" ;;
    esac
    echo "kraal-$arch-$os"
}

# Download release $1 into $dir/$1, checked against its checksums.
download() {
    name=$(asset)
    base="https://github.com/$repo/releases/download/$1"
    tmp=$(mktemp -d)
    say "downloading $name.tar.gz ($1)"
    curl -fsSL --proto '=https' -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" || fail "could not download $base/$name.tar.gz"
    curl -fsSL --proto '=https' -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || fail "could not download $base/SHA256SUMS"
    want=$(awk -v a="$name.tar.gz" '$2 == a { print $1 }' "$tmp/SHA256SUMS")
    got=$(sha256 "$tmp/$name.tar.gz")
    if [ -z "$want" ] || [ "$want" != "$got" ]; then
        fail "$name.tar.gz of $1 does not match its checksum"
    fi
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
    mkdir -p "$dir/$1"
    mv "$tmp/$name/kraal" "$dir/$1/kraal"
    rm -rf "$tmp"
}

# Build release $1 from its tag into $dir/$1-built.
build() {
    git -C "$root" rev-parse -q --verify "refs/tags/$1" >/dev/null || fail "no tag $1 here: git fetch --tags"
    src="$dir/$1-built/src"
    rm -rf "$src"
    mkdir -p "$src"
    git -C "$root" archive "$1" | tar -x -C "$src"
    cargo build --locked --manifest-path "$src/Cargo.toml" --target-dir "$dir/$1-built/target"
    cp "$dir/$1-built/target/debug/kraal" "$dir/$1-built/kraal"
}

cd "$root"
for tag in $tags; do
    if [ "$build" = 1 ]; then
        bin="$dir/$tag-built/kraal"
        [ -x "$bin" ] || build "$tag"
    else
        bin="$dir/$tag/kraal"
        [ -x "$bin" ] || download "$tag"
    fi
    # In an empty environment: only the tests, which have directories of their own, run a
    # release where it could read something. Here it would be the developer's.
    version=$(env -i "$bin" --version) || fail "$bin does not run here"
    say "this kraal with $version"
    KRAAL_OLD="$bin" cargo test --locked --test compat -- --ignored "$@"
done
