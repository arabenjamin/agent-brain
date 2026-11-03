#!/bin/sh
# Fetch and verify every file in the manifest into /models/gguf. Runs inside the
# `models-fetch` compose service. A file already there with the right sha256 is
# kept; a partial download resumes; a wrong sha256 is deleted and fails the run.
set -eu
MANIFEST="${1:-/manifest}"
DIR=/models/gguf
mkdir -p "$DIR"

ok() { [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]; }

failed=0
while read -r file sha bytes license url; do
    case "$file" in ''|\#*) continue ;; esac
    if [ -e "$DIR/$file" ]; then
        if ok "$DIR/$file" "$sha"; then
            echo "$file: already here, sha256 matches"
            continue
        fi
        echo "$file: sha256 differs from the manifest, fetching again"
        rm -f "${DIR:?}/${file:?}"
    fi
    echo "$file: $bytes bytes, $license, from $url"
    if ! curl -fL --retry 5 --retry-delay 10 --retry-all-errors -C - -sS -o "$DIR/$file.part" "$url"; then
        echo "$file: download failed; run again to resume" >&2
        failed=1
        continue
    fi
    if ok "$DIR/$file.part" "$sha"; then
        mv "$DIR/$file.part" "$DIR/$file"
        echo "$file: verified"
    else
        rm -f "${DIR:?}/${file:?}.part"
        echo "$file: sha256 does not match the manifest; deleted" >&2
        failed=1
    fi
done < "$MANIFEST"
exit "$failed"
