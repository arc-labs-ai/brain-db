# Bulk encoding from files + stdin

The shell does one encode per invocation. For thousands of memories,
spinning up a process per encode is wasteful — but the shell isn't
the right tool for very high throughput either; for that, use the
[Rust SDK](../../reference/sdk-rust.md) directly.

This guide covers the middle ground: 10 to ~10K memories from
files / pipelines / scripts. The patterns scale to ~100 encodes/sec
on a developer machine.

For workflows where you need higher throughput, see the **"When to
switch to the SDK"** section at the bottom.

---

## 1. One-per-line file

The simplest pattern. `lines.txt` has one memory per line:

```text
Alice merged the auth-rewrite branch
auth tokens now use BLAKE3 instead of SHA-1
the auth-rewrite was triggered by a security audit in Q3
```

Bash loop:

```bash
while IFS= read -r line; do
    brain --agent demo encode "$line" --context 7
done < lines.txt
```

Output (table mode, one per line):

```
ok  s2/m1/v1  lsn=1
    agent=… · ctx=7 · episodic · sal=0.500 · fp=…
ok  s2/m2/v1  lsn=2
    agent=… · ctx=7 · episodic · sal=0.500 · fp=…
ok  s2/m3/v1  lsn=3
    agent=… · ctx=7 · episodic · sal=0.500 · fp=…
```

---

## 2. Capture every memory_id

For follow-up linking / forgetting, dump just the ids via JSON:

```bash
while IFS= read -r line; do
    brain --agent demo --output json encode "$line" --context 7 \
        | jq -r '.result.memory_id'
done < lines.txt > memory_ids.txt
```

Resulting `memory_ids.txt`:

```
0x00020000000000010000000100000000
0x00020000000000020000000100000000
0x00020000000000030000000100000000
```

Now you can `paste` them with the original text, link them in
sequence with `--kind followed-by`, etc.

---

## 3. Linking sequentially-encoded memories

Encode + chain adjacent memories with `followed-by`:

```bash
prev=""
while IFS= read -r line; do
    id=$(brain --agent demo --output json encode "$line" --context 7 \
           | jq -r '.result.memory_id')
    if [[ -n "$prev" ]]; then
        brain --agent demo link "$prev" followed-by "$id" --weight 1.0
    fi
    prev="$id"
done < lines.txt
```

Caveats:

- `link` is itself a round-trip; this doubles the per-line cost.
  Use a `txn` to batch (next pattern).
- If an encode fails, the `link` would fire with a stale `prev`.
  Add `set -e` to abort on first failure.

---

## 4. Atomic batches with `txn`

For N memories that should all-or-nothing, use a transaction. The
REPL gives you sticky-txn semantics:

```bash
brain --agent demo <<EOF
txn begin
encode "first batched memory"
encode "second batched memory"
encode "third batched memory"
txn commit
EOF
```

The REPL stickys the txn_id after `txn begin`, so each subsequent
encode auto-attaches `--txn`. `txn commit` applies all in one redb
write transaction + one WAL bracket (`TxnBegin` → ops →
`TxnCommit`). A crash between buffering and commit drops the whole
batch.

In a script that builds the input programmatically:

```bash
{
    echo "txn begin"
    while IFS= read -r line; do
        # Properly quote each line for the REPL parser.
        printf 'encode %q --context 7\n' "$line"
    done < lines.txt
    echo "txn commit"
} | brain --agent demo
```

---

## 5. CSV with context per row

For tabular input where each row carries text + context:

```csv
text,context,kind,salience
Alice merged auth-rewrite,7,episodic,0.7
auth tokens use BLAKE3,7,semantic,0.9
security audit triggered rewrite,7,episodic,0.6
```

Bash + `awk`:

```bash
tail -n +2 input.csv | awk -F, '{
    printf("encode %s --context %s --kind %s --salience %s\n", $1, $2, $3, $4)
}' | brain --agent demo
```

(Quoting gets ugly with CSV text that contains commas — for serious
work, drop to Python or the SDK.)

---

## 6. Throughput tips

- **Use the REPL pipe** (one process, sticky session) over per-
  encode invocation. Saves the ~50ms startup cost per call.
- **`txn` batches** amortise the redb commit. ~50 encodes per
  commit is a good rule of thumb.
- **`--deduplicate`** for idempotent re-runs — if your input
  contains duplicates, the second copy is a fingerprint hit
  instead of a fresh slot.
- **`--context` namespaces:** put bulk-imports in a dedicated
  context (e.g. `context_id = 9999`) so a bad import is easy to
  recall + forget en masse.

---

## 7. JSON-streaming output for downstream consumers

When you're piping into another tool, use `--output json` so each
line is a valid JSON document:

```bash
while IFS= read -r line; do
    brain --agent demo --output json encode "$line" --context 7
done < lines.txt > encode.jsonl
```

`encode.jsonl` is JSON-lines (one document per line) — feed to
`jq`, `python -c`, ClickHouse, etc.

---

## When to switch to the SDK

The shell tops out around ~100 encodes/sec on a developer machine
because of fork/exec overhead per invocation. The REPL pipe doubles
that. For higher throughput:

- **1K encodes/sec** — pipe through the REPL with `txn` batches of 100.
- **10K encodes/sec or higher** — drop to the [Rust SDK](../../reference/sdk-rust.md).
  One process, many concurrent encodes, no parse-print round-trip.

The SDK and the shell speak the same wire protocol, so a script
that proves the shape with the shell then ports to the SDK without
behavior change.

---

## See also

- [`scripting-with-json.md`](scripting-with-json.md) — jq pipelines
- [`subscribe-and-replay.md`](subscribe-and-replay.md) — observe the encodes you just made
- [`../../reference/sdk-rust.md`](../../reference/sdk-rust.md) — for >1K encodes/sec
- [`../../reference/shell/commands.md#txn`](../../reference/shell/commands.md#txn) — txn reference
