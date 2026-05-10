#!/usr/bin/env bash
# Pre-bash safety hook for autonomous Claude Code.
#
# Reads a JSON tool-use payload on stdin, exits 0 to allow, non-zero to block.
# Triggered before any Bash tool invocation.
#
# This runs even in --dangerously-skip-permissions mode and is the safety
# net for genuinely-destructive operations.

set -euo pipefail

# Read the JSON payload (stdin per Claude Code hook protocol).
PAYLOAD=$(cat)

# Extract the command. Best-effort: tries jq first, falls back to grep.
CMD=$(echo "$PAYLOAD" | jq -r '.tool_input.command // empty' 2>/dev/null || true)
if [ -z "$CMD" ]; then
  CMD=$(echo "$PAYLOAD" | grep -oE '"command"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"command"[[:space:]]*:[[:space:]]*"\(.*\)"/\1/' || true)
fi

# Patterns that are NEVER allowed, even autonomously.
DENY_PATTERNS=(
  # Destructive filesystem
  'rm[[:space:]]+-rf[[:space:]]+/[[:space:]]*$'
  'rm[[:space:]]+-rf[[:space:]]+/\*'
  'rm[[:space:]]+-rf[[:space:]]+~'
  'rm[[:space:]]+-rf[[:space:]]+\$HOME'
  'rm[[:space:]]+-rf[[:space:]]+\.\.'
  ':\(\)\s*\{\s*:\|:&\s*\};:'   # fork bomb
  # Disk/device writes
  'dd[[:space:]]+.*of=/dev/'
  'mkfs\.'
  'fdisk[[:space:]]'
  # Package mutations on the system
  'sudo[[:space:]]'
  'apt[[:space:]]+(install|remove|purge)'
  'apt-get[[:space:]]+(install|remove|purge)'
  'dpkg[[:space:]]+-i'
  # Git destructive
  'git[[:space:]]+push[[:space:]]+.*--force'
  'git[[:space:]]+push[[:space:]]+.*-f([[:space:]]|$)'
  'git[[:space:]]+push[[:space:]]+--mirror'
  'git[[:space:]]+reset[[:space:]]+--hard[[:space:]]+HEAD~[0-9]+'
  # Cargo destructive
  'cargo[[:space:]]+publish'
  'cargo[[:space:]]+yank'
  'cargo[[:space:]]+owner'
  # Spec mutation via shell (defense in depth)
  'rm[[:space:]]+.*spec/'
  '>[[:space:]]*spec/'
  'sed[[:space:]]+-i.*spec/'
  'mv[[:space:]]+.*[[:space:]]+spec/'
  # Network exfiltration of secrets
  'curl[[:space:]].*\$\{?[A-Z_]*(KEY|TOKEN|SECRET|PASSWORD)'
  # Credential probing
  'cat[[:space:]]+.*\.ssh/'
  'cat[[:space:]]+.*\.aws/credentials'
  'cat[[:space:]]+.*\.netrc'
)

for pattern in "${DENY_PATTERNS[@]}"; do
  if echo "$CMD" | grep -qE "$pattern"; then
    echo "BLOCKED by .claude/hooks/pre-bash.sh: command matches denied pattern" >&2
    echo "Pattern: $pattern" >&2
    echo "Command: $CMD" >&2
    echo "" >&2
    echo "If this block is wrong, edit .claude/hooks/pre-bash.sh." >&2
    echo "If you intended this destructive action, run it manually outside Claude." >&2
    exit 2
  fi
done

# Patterns that warn but allow. Currently empty — add for visibility-only checks.

# Default: allow.
exit 0
