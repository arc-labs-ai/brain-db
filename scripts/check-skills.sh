#!/usr/bin/env bash
#
# Validate every .claude/skills/<name>/SKILL.md per the conventions in
# .claude/skills/CONVENTIONS.md.
#
# Checks (per skill):
#   1. SKILL.md exists.
#   2. YAML frontmatter is bracketed by `---` lines at the top of the file.
#   3. Required frontmatter keys are present: name, description, when-to-use.
#   4. `name` matches the parent directory name.
#   5. `description` is non-empty and ≤ 200 chars.
#   6. Every path in `spec-refs:` exists on disk.
#   7. If `source:` is set (vendored skill), `license:` is also set.
#
# Exit code: 0 if all skills are valid, 1 otherwise.

set -euo pipefail

cd "$(dirname "$0")/.."

skills_dir=".claude/skills"
errors=0
checked=0

if [[ ! -d "$skills_dir" ]]; then
    echo "✓ no .claude/skills/ directory — nothing to check"
    exit 0
fi

# Helper: extract a single-line value for `key:` from the SKILL.md
# frontmatter (the block between the *first* two `---` lines, not any
# `---` separators that appear later in the body).
# Returns empty string if not found.
extract_scalar() {
    local file="$1" key="$2"
    awk -v key="$key" '
        NR == 1 && /^---$/ { in_fm = 1; next }
        in_fm && /^---$/ { exit }
        in_fm && $0 ~ "^"key":[[:space:]]" {
            sub("^"key":[[:space:]]*", "")
            sub("[[:space:]]*$", "")
            print
            exit
        }
    ' "$file"
}

# Helper: extract list items under `spec-refs:` (one per line, in order).
extract_spec_refs() {
    local file="$1"
    awk '
        NR == 1 && /^---$/ { in_fm = 1; next }
        in_fm && /^---$/ { exit }
        in_fm && /^spec-refs:[[:space:]]*$/ { in_list = 1; next }
        in_fm && in_list && /^[[:space:]]*-[[:space:]]/ {
            sub(/^[[:space:]]*-[[:space:]]*/, "")
            sub(/[[:space:]]*$/, "")
            print
            next
        }
        in_fm && in_list && !/^[[:space:]]/ { in_list = 0 }
    ' "$file"
}

# Helper: report an error in a skill.
fail() {
    local skill="$1" msg="$2"
    echo "✗ $skill: $msg"
    errors=$((errors + 1))
}

# Iterate over each skill folder.
for skill_path in "$skills_dir"/*/; do
    [[ -d "$skill_path" ]] || continue
    skill_name="$(basename "$skill_path")"
    checked=$((checked + 1))

    skill_md="${skill_path}SKILL.md"

    # 1. SKILL.md must exist.
    if [[ ! -f "$skill_md" ]]; then
        fail "$skill_name" "missing SKILL.md"
        continue
    fi

    # 2. Frontmatter must be bracketed by --- lines at the very top.
    if ! head -1 "$skill_md" | grep -qx -- '---'; then
        fail "$skill_name" "SKILL.md does not start with '---' (no frontmatter)"
        continue
    fi
    if ! awk 'NR==1 && /^---$/ { found_first=1; next }
              found_first && /^---$/ { print "ok"; exit }' "$skill_md" | grep -q '^ok$'; then
        fail "$skill_name" "SKILL.md frontmatter is not closed with '---'"
        continue
    fi

    # 3. Required keys.
    name="$(extract_scalar "$skill_md" name)"
    description="$(extract_scalar "$skill_md" description)"
    when_to_use_present="$(awk '/^---$/ { in_fm = !in_fm; next }
                                 in_fm && /^when-to-use:/ { print "yes"; exit }' "$skill_md")"

    [[ -n "$name" ]] || fail "$skill_name" "frontmatter missing 'name'"
    [[ -n "$description" ]] || fail "$skill_name" "frontmatter missing 'description'"
    [[ -n "$when_to_use_present" ]] || fail "$skill_name" "frontmatter missing 'when-to-use'"

    # 4. Name matches folder.
    if [[ -n "$name" && "$name" != "$skill_name" ]]; then
        fail "$skill_name" "frontmatter name='$name' does not match folder name"
    fi

    # 5. Description ≤ 200 chars.
    if [[ ${#description} -gt 200 ]]; then
        fail "$skill_name" "description is ${#description} chars (max 200)"
    fi

    # 6. spec-refs: paths exist.
    while IFS= read -r ref; do
        [[ -z "$ref" ]] && continue
        if [[ ! -e "$ref" ]]; then
            fail "$skill_name" "spec-refs path does not exist: $ref"
        fi
    done < <(extract_spec_refs "$skill_md")

    # 7. Vendored skills: source ⇒ license.
    source_url="$(extract_scalar "$skill_md" source)"
    if [[ -n "$source_url" ]]; then
        license="$(extract_scalar "$skill_md" license)"
        if [[ -z "$license" ]]; then
            fail "$skill_name" "vendored skill (source: set) is missing 'license' key"
        fi
    fi
done

echo
if [[ $errors -eq 0 ]]; then
    echo "✓ all $checked skill(s) valid"
    exit 0
else
    echo "✗ $errors error(s) across $checked skill(s)"
    exit 1
fi
