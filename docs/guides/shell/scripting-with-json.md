# Scripting with `brain --output json` + `jq`

The shell's JSON mode is line-delimited (one document per command),
which makes it trivial to feed into `jq`, `python -m json.tool`,
ClickHouse, or any other line-oriented JSON consumer.

This guide is a recipe collection. For the canonical JSON shapes,
see
[`../../reference/shell/output-formats.md`](../../reference/shell/output-formats.md).

---

## The envelope

Every command's JSON output is wrapped:

```json
{ "op": "<verb>", "result": <body> }
```

The envelope is stable across commands. The `<body>` shape
depends on the verb — see the reference for each.

---

## 1. Extract one field

```bash
# Just the memory_id from an encode.
brain --agent demo --output json encode "hello" \
  | jq -r '.result.memory_id'
# → 0x00020000000000010000000100000000

# Just the LSN.
brain --agent demo --output json encode "hello" \
  | jq -r '.result.lsn'
# → 1

# Both, in shell variables.
read -r MID LSN < <(brain --agent demo --output json encode "hello" \
  | jq -r '"\(.result.memory_id) \(.result.lsn)"')
echo "memory $MID at lsn $LSN"
```

---

## 2. Filter recall results

```bash
# Memories above a similarity threshold.
brain --agent demo --output json recall "auth" --top-k 10 \
  | jq '.result[] | select(.similarity_score > 0.05)'

# Just IDs + scores, as a table.
brain --agent demo --output json recall "auth" --top-k 10 --include-text \
  | jq -r '.result[] | "\(.similarity_score)\t\(.memory_id)\t\(.text)"'
# → 0.0164  0x000…  Alice merged the auth-rewrite branch
# → 0.0161  0x000…  auth tokens now use BLAKE3 instead of SHA-1

# Only consolidated rows.
brain --agent demo --output json recall "anything" --top-k 100 \
  | jq '.result[] | select(.consolidated_at_unix_nanos != null)'

# Hottest memories (highest access_count).
brain --agent demo --output json recall "" --top-k 100 \
  | jq '.result | sort_by(-.access_count) | .[0:5]'
```

---

## 3. Stream subscribe events through `jq`

Subscribe is streaming JSON — one event per line, flushed:

```bash
brain --agent demo --output json subscribe \
  | jq -c '{lsn, event_type, memory_id}'

# Live output:
# {"lsn":1,"event_type":"Encoded","memory_id":"0x..."}
# {"lsn":2,"event_type":"Encoded","memory_id":"0x..."}
# {"lsn":3,"event_type":"Forgotten","memory_id":"0x..."}
```

Filter to only `Forgotten` events:

```bash
brain --agent demo --output json subscribe \
  | jq -c 'select(.result.event_type == "Forgotten")'
```

Count events per minute (rolling):

```bash
brain --agent demo --output json subscribe \
  | jq -c '{minute: (.result.timestamp_unix_nanos / 60000000000 | floor)}' \
  | sort | uniq -c
```

---

## 4. The `recall → subscribe` chain

```bash
# Find the memory.
LSN=$(brain --agent demo --output json recall "anchor" --top-k 1 \
       | jq '.result[0].lsn')

# Follow everything after it.
brain --agent demo --output json subscribe --start-lsn $((LSN+1)) \
  | jq -c '{lsn, type: .result.event_type}'
```

See [`subscribe-and-replay.md`](subscribe-and-replay.md) for more.

---

## 5. Robust error handling in scripts

JSON output mixes success and error in the envelope — successful
commands have `{ "op": "...", "result": ... }`, errors have
`{ "op": "error", "result": { ... } }`. Detect both:

```bash
resp=$(brain --agent demo --output json encode "hello")
if echo "$resp" | jq -e '.op == "error"' > /dev/null; then
    code=$(echo "$resp" | jq -r '.result.code')
    msg=$(echo "$resp" | jq -r '.result.message')
    echo "encode failed [$code]: $msg" >&2
    exit 1
fi
mid=$(echo "$resp" | jq -r '.result.memory_id')
echo "encoded $mid"
```

Or use the shell's exit code (non-zero on error):

```bash
if ! resp=$(brain --agent demo --output json encode "hello"); then
    echo "encode failed (exit $?)" >&2
    exit 1
fi
```

Both work; the first gives you the structured error code.

---

## 6. Generating bulk input from a CSV

```bash
# input.csv:
#   text,context
#   "hello",1
#   "world",1

tail -n +2 input.csv \
  | jq -R 'split(",") | {text: .[0], context: (.[1] | tonumber)}' \
  | jq -c .  \
  | while read -r row; do
        text=$(echo "$row" | jq -r .text)
        ctx=$(echo "$row" | jq -r .context)
        brain --agent demo --output json encode "$text" --context "$ctx"
    done \
  > encoded.jsonl
```

Result is `encoded.jsonl` — one document per encoded memory; pipe
to ClickHouse or further `jq`.

---

## 7. Pretty-printing in interactive use

When you want JSON output but human-readable:

```bash
brain --agent demo --output json recall "foo" --top-k 3 \
  | jq .
```

(Defaults inside a TTY are still `--output table`, but explicit
`--output json` paired with `| jq .` gives you the structured view
when scanning for an exact field.)

---

## 8. CSV / TSV output via `jq`

`brain` itself only emits table or JSON. For CSV-style output,
`jq -r` works:

```bash
brain --agent demo --output json recall "foo" --top-k 10 --include-text \
  | jq -r '.result[] | [.memory_id, .similarity_score, .context_id, .text] | @csv'
```

Headers + body:

```bash
{
    echo "memory_id,score,context,text"
    brain --agent demo --output json recall "foo" --top-k 10 --include-text \
      | jq -r '.result[] | [.memory_id, .similarity_score, .context_id, .text] | @csv'
} > recall.csv
```

---

## 9. Idiom: `jq -e` for assertions

`jq -e` exits non-zero if the filter produces `false` or `null` —
perfect for shell-script assertions:

```bash
# Assert: at least 1 result.
brain --agent demo --output json recall "x" \
  | jq -e '.result | length >= 1' > /dev/null \
  || { echo "no recall results" >&2; exit 1; }

# Assert: a specific memory_id is in the results.
brain --agent demo --output json recall "x" --top-k 100 \
  | jq -e --arg id "0x..." '.result | any(.memory_id == $id)' > /dev/null \
  || { echo "expected memory missing" >&2; exit 1; }
```

---

## See also

- [`../../reference/shell/output-formats.md`](../../reference/shell/output-formats.md) — per-verb JSON schemas
- [`bulk-encode.md`](bulk-encode.md) — high-volume encode patterns
- [`subscribe-and-replay.md`](subscribe-and-replay.md) — event streaming
- [`troubleshooting.md`](troubleshooting.md) — debugging script flows
