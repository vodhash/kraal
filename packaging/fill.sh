#!/bin/sh
# A packaging template for a release: @VERSION@, and @SHA256 <target>@ from its SHA256SUMS. Only
# the four archives a release has, each with a SHA-256 sum, are read: SHA256SUMS is not trusted
# to say anything else.
# Usage: packaging/fill.sh TEMPLATE VERSION SHA256SUMS
set -eu
template=$1 version=$2 sums=$3
case $version in
*[!0-9.]* | "" | .* | *. | *..*) echo "fill.sh: not a version: $version" >&2; exit 1 ;;
esac
script="s/@VERSION@/$version/g"
while read -r sum file; do
    case $file in
    kraal-x86_64-unknown-linux-musl.tar.gz | kraal-aarch64-unknown-linux-musl.tar.gz | \
        kraal-x86_64-apple-darwin.tar.gz | kraal-aarch64-apple-darwin.tar.gz) ;;
    *) echo "fill.sh: not an archive of a release: $file" >&2; exit 1 ;;
    esac
    if [ ${#sum} -ne 64 ] || [ -n "$(printf '%s' "$sum" | tr -d 0-9a-f)" ]; then
        echo "fill.sh: not a SHA-256 sum for $file" >&2
        exit 1
    fi
    target=${file#kraal-}
    target=${target%.tar.gz}
    script="$script;s/@SHA256 $target@/$sum/g"
done < "$sums"
out=$(sed "$script" "$template")
if printf '%s\n' "$out" | grep -q '@VERSION@\|@SHA256 '; then
    echo "fill.sh: $sums lacks an archive $template names" >&2
    exit 1
fi
printf '%s\n' "$out"
