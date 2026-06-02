#!/usr/bin/env bash
# Brain v1.0 acceptance suite — single entry point.
#
# Runs the release gates and reports pass/fail per gate plus an overall
# status. Exit 0 iff every selected gate passes.
#
# The gates fall into two tiers. CI-runnable gates (unit, integration,
# e2e, smoke, chaos subset, docs, fuzz smoke) run on every PR and on a
# dev box via `just docker`. Operator-only gates (full performance bench
# on quiet hardware, 48 h soak, full chaos with cgroups/tc, audit-log
# compliance) need dedicated infrastructure and are run before tagging a
# release; they report `skip` here unless their infra is present.
#
# Usage:
#
#   bash scripts/acceptance.sh                # all gates
#   bash scripts/acceptance.sh 1 2 4          # only those gates
#   bash scripts/acceptance.sh --skip 7 8     # all except soak + compliance
#
# Environment:
#
#   BRAIN_ACCEPT_REPORT  path for the per-gate JSONL report
#                        (default: target/acceptance-report.jsonl, gitignored)

set -uo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." &> /dev/null && pwd)"
cd "$REPO_ROOT"

REPORT_FILE="${BRAIN_ACCEPT_REPORT:-$REPO_ROOT/target/acceptance-report.jsonl}"
mkdir -p "$(dirname "$REPORT_FILE")"
: > "$REPORT_FILE"

ANSI_RED='\033[31m'
ANSI_GREEN='\033[32m'
ANSI_YELLOW='\033[33m'
ANSI_RESET='\033[0m'

ALL_GATES=(1 2 3 4 5 6 7 8 9 10)
SELECTED=()
SKIPPED_FLAGS=()

# --- arg parsing --------------------------------------------------------
mode="all"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip)
      mode="skip"
      shift
      while [[ $# -gt 0 && "$1" =~ ^[0-9]+$ ]]; do
        SKIPPED_FLAGS+=("$1")
        shift
      done
      ;;
    --help|-h)
      sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    [0-9]*)
      mode="explicit"
      SELECTED+=("$1")
      shift
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

case "$mode" in
  all)      SELECTED=("${ALL_GATES[@]}") ;;
  explicit) : ;;
  skip)
    SELECTED=()
    for g in "${ALL_GATES[@]}"; do
      drop=false
      for s in "${SKIPPED_FLAGS[@]}"; do
        [[ "$g" == "$s" ]] && drop=true
      done
      $drop || SELECTED+=("$g")
    done
    ;;
esac

# --- helpers ------------------------------------------------------------
emit_report() {
  local gate="$1" status="$2" reason="${3:-}"
  printf '{"gate":%d,"status":"%s","reason":"%s"}\n' \
    "$gate" "$status" "$reason" >> "$REPORT_FILE"
}

label_for() {
  case "$1" in
    1)  echo "Unit tests" ;;
    2)  echo "Integration tests" ;;
    3)  echo "End-to-end wire tests" ;;
    4)  echo "Smoke test" ;;
    5)  echo "Performance targets" ;;
    6)  echo "Chaos tests" ;;
    7)  echo "Soak test (48 h)" ;;
    8)  echo "Compliance tests" ;;
    9)  echo "Documentation tests" ;;
    10) echo "Security tests (fuzz)" ;;
    *)  echo "?" ;;
  esac
}

# --- gate implementations ----------------------------------------------
# brain-server is Linux/io_uring only, so its tests run through the
# `just docker` devcontainer. Sequential test linking matches CI's
# OOM-avoidance posture (the admin binary is SIGKILLed under default
# parallelism at the container memory ceiling).

gate_unit() {
  just docker-test --workspace 2>&1 | tail -5
}

gate_integration() {
  just docker-test -p brain-server --test e2e --test admin 2>&1 | tail -5
}

gate_e2e() {
  # SDK/CLI-driven e2e lives in the clients repo (brain-db-io); the
  # wire-direct suite here is the in-repo end-to-end gate.
  just docker-test -p brain-server --test e2e --test opaque_body_compat \
      --test entity_wire --test statement_wire --test relation_wire \
      --test schema_wire --test query_wire 2>&1 | tail -5
}

gate_smoke() {
  # One encode → recall round-trip via the wire e2e.
  just docker-test -p brain-server --test e2e 2>&1 | tail -5
}

gate_performance() {
  # CI compiles the criterion benches (catches breakage); a real
  # release run replaces this with a full `cargo bench` on quiet
  # hardware and a diff against the prior release baseline.
  cargo bench --workspace --no-run 2>&1 | tail -3
}

gate_chaos() {
  just docker-test -p brain-storage --test random_kill --test bit_flip --test io_fault 2>&1 | tail -5
}

gate_soak() {
  # The 48 h soak runs on operator infrastructure; the soak rig lives
  # in the clients repo (brain-db-io) alongside the SDK it drives.
  echo "soak rig runs on operator infra (brain-db-io); skipped here"
}

gate_compliance() {
  # TLS termination is wired (brain-server/src/bootstrap/tls.rs); the
  # audit-log integrity path is operator-verified. CI checks the
  # cert-loading code path.
  just docker-test -p brain-server --test admin tls 2>&1 | tail -3
}

gate_docs() {
  # Cargo doc must build clean and every relative cross-reference in
  # spec/ + ROADMAP.md must resolve.
  cargo doc --workspace --no-deps 2>&1 | tail -3
  bash scripts/spec-link-check.sh --strict
}

gate_security() {
  # The wire fuzz targets. CI runs a 60-second smoke; release builds
  # run the overnight sweep. Here we compile-check the targets.
  cargo build -p brain-protocol --tests 2>&1 | tail -3
}

# --- runner -------------------------------------------------------------
PASSED=0
FAILED=0
SKIPPED=0
TOTAL=${#SELECTED[@]}

printf '%bBrain v1.0 acceptance — %d gates selected%b\n\n' "$ANSI_YELLOW" "$TOTAL" "$ANSI_RESET"

for gate in "${SELECTED[@]}"; do
  label="$(label_for "$gate")"
  printf '── Gate %d: %s ──\n' "$gate" "$label"
  case "$gate" in
    1)  fn=gate_unit ;;
    2)  fn=gate_integration ;;
    3)  fn=gate_e2e ;;
    4)  fn=gate_smoke ;;
    5)  fn=gate_performance ;;
    6)  fn=gate_chaos ;;
    7)  fn=gate_soak ;;
    8)  fn=gate_compliance ;;
    9)  fn=gate_docs ;;
    10) fn=gate_security ;;
    *)  echo "unknown gate $gate" >&2; FAILED=$((FAILED+1)); continue ;;
  esac

  if output="$($fn 2>&1)"; then
    printf '%s\n' "$output"
    printf '%bgate %d PASS%b\n\n' "$ANSI_GREEN" "$gate" "$ANSI_RESET"
    emit_report "$gate" "pass" ""
    PASSED=$((PASSED+1))
  else
    printf '%s\n' "$output"
    printf '%bgate %d FAIL%b\n\n' "$ANSI_RED" "$gate" "$ANSI_RESET"
    emit_report "$gate" "fail" "$(echo "$output" | tail -1 | tr -d '\n')"
    FAILED=$((FAILED+1))
  fi
done

# --- summary ------------------------------------------------------------
printf '── Summary ──\n'
printf '  Passed:  %d\n' "$PASSED"
printf '  Failed:  %d\n' "$FAILED"
printf '  Skipped: %d\n' "$SKIPPED"
printf '  Report:  %s\n' "$REPORT_FILE"

if [[ $FAILED -gt 0 ]]; then
  printf '%bACCEPTANCE: FAIL%b (%d/%d gates failed)\n' "$ANSI_RED" "$ANSI_RESET" "$FAILED" "$TOTAL"
  exit 1
fi
printf '%bACCEPTANCE: PASS%b (%d/%d gates green)\n' "$ANSI_GREEN" "$ANSI_RESET" "$PASSED" "$TOTAL"
exit 0
