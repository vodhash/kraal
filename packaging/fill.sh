#!/bin/sh
# A packaging template for a release: @VERSION@, and @SHA256 <target>@ from its SHA256SUMS.
# Usage: packaging/fill.sh TEMPLATE VERSION SHA256SUMS
set -eu
template=$1 version=$2 sums=$3
case $version in
*[!0-9.]* | "" | .* | *.) echo "fill.sh: not a version: $version" >&2; exit 1 ;;
esac
script="s/@VERSION@/$version/g"
while read -r sum file; do
    target=${file#kraal-}
    target=${target%.tar.gz}
    case $sum in *[!0-9a-f]* | "") echo "fill.sh: not a checksum for $file" >&2; exit 1 ;; esac
    script="$script;s/@SHA256 $target@/$sum/g"
done < "$sums"
out=$(sed "$script" "$template")
if printf '%s\n' "$out" | grep -q '@VERSION@\|@SHA256 '; then
    echo "fill.sh: $sums lacks an archive $template names" >&2
    exit 1
fi
printf '%s\n' "$out"
