#!/usr/bin/env bash
# Validates that every `[`./…`]` relative cross-ref in spec/ + ROADMAP.md
# resolves to an existing file. A broken link means the prose points at a
# file that was moved or deleted.

set -euo pipefail

cd "$(dirname "$0")/.."

broken=0
while IFS=: read -r path line rest; do
    # Pull the target out of the matched line. We match
    # patterns like [`./00_foo.md`](./00_foo.md) — extract
    # the path inside the parens.
    targets=$(echo "$rest" | grep -oE '\]\([^)]+\)' | sed -E 's/\]\(([^)]+)\)/\1/')
    for target in $targets; do
        # Skip absolute URLs.
        [[ "$target" =~ ^https?:// ]] && continue
        [[ "$target" == \#* ]] && continue
        # Strip any `#anchor` suffix.
        target="${target%%#*}"
        if [[ "$target" != /* ]]; then
            target_path="$(dirname "$path")/$target"
        else
            target_path="${target#/}"
        fi
        if [[ ! -e "$target_path" ]]; then
            echo "BROKEN: $path:$line → $target"
            broken=$((broken + 1))
        fi
    done
done < <(grep -RIn -E '\]\(\./[^)]+\)' spec/ ROADMAP.md 2>/dev/null || true)

if (( broken > 0 )); then
    if [[ "${1:-}" == "--strict" ]]; then
        echo "Found $broken broken link(s); failing (strict mode)."
        exit 1
    fi
    echo "Found $broken broken link(s); report-only (pass --strict to fail)."
    exit 0
fi
echo "All spec cross-refs resolve."
