---
description: Read a specific Brain specification section
argument-hint: <spec-number> [filename-or-keyword]
allowed-tools: Read, Glob, Grep
---

Read the relevant Brain specification section based on the user's input: `$ARGUMENTS`

The argument format is `<spec-number> [filename-or-keyword]`.

The spec lives under `spec/` with directories named like `00_master_overview/`, `01_system_architecture/`, ..., `16_benchmarks_acceptance/`.

Steps:

1. **Identify the spec directory.** Match the spec number (e.g. `5` or `05`) to its directory by looking for `spec/0?<num>_*/`. If the user gave just a number, list that directory's contents to identify which file they likely want.

2. **Identify the file.** If the user supplied a second argument:
   - If it looks like a filename (e.g. `06_recovery`, `02_frame_format`), match it against the directory's files (numbered `00_purpose.md`, `01_*.md`, etc.).
   - If it looks like a keyword, grep within the directory for the most relevant file.
   - If unclear, list the directory's files and ask the user which one they want — but only ask if truly ambiguous.

3. **Read the file** and return a clean summary, plus key excerpts. Don't dump the whole file unless it's short (< 100 lines).

4. **Cross-reference.** If the file references other spec sections (e.g. "See spec/07/03"), mention them so the user knows where to look next.

5. If no argument was given, list the 17 top-level spec directories so the user knows what's available.

Be concise. The user knows what Brain is — they want spec content, not a re-explanation.
