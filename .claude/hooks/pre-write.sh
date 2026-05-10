#!/usr/bin/env bash
# Pre-write/pre-edit safety hook for autonomous Claude Code.
#
# Blocks writes to read-only paths: spec/, .git/, target/.
# Triggered before any Write or Edit tool invocation.

set -euo pipefail

PAYLOAD=$(cat)

# Path is in tool_input.file_path for Write/Edit/MultiEdit.
PATH_TO_WRITE=$(echo "$PAYLOAD" | jq -r '.tool_input.file_path // empty' 2>/dev/null || true)
if [ -z "$PATH_TO_WRITE" ]; then
  # Fallback: try generic match.
  PATH_TO_WRITE=$(echo "$PAYLOAD" | grep -oE '"file_path"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"file_path"[[:space:]]*:[[:space:]]*"\(.*\)"/\1/' || true)
fi

# Read-only paths.
READONLY_PATTERNS=(
  '/spec/'
  '/\.git/'
  '/target/'
)

for pattern in "${READONLY_PATTERNS[@]}"; do
  if echo "$PATH_TO_WRITE" | grep -qE "$pattern"; then
    echo "BLOCKED by .claude/hooks/pre-write.sh: path is read-only for autonomous Claude" >&2
    echo "Path: $PATH_TO_WRITE" >&2
    echo "Pattern: $pattern" >&2
    echo "" >&2
    echo "Spec changes require explicit user action. .git/ and target/ are not user-editable." >&2
    exit 2
  fi
done

exit 0
