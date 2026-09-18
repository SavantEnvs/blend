#!/usr/bin/env bash
#
# blend/mayhem/test.sh — RUN lukebitts/blend's own test suite (`cargo test`: the src/lib.rs
# doc-test, which parses examples/blend_files/3_5.blend and iterates its objects) PLUS a
# known-answer oracle over the crate's shipped example (names_positions run against the four
# committed .blend files, output diffed against a committed golden file). Emits a CTRF summary;
# exit 0 iff nothing failed.
#
# Upstream ships no tests/ directory and no #[test] unit tests — its entire executable suite is
# the one rustdoc doc-test in src/lib.rs. `cargo test` runs ALL of it (lib harness + doc-tests).
# Because that is a single test, the names_positions known-answer check (authored, but built
# entirely from upstream's own example + upstream's own .blend corpus) is added so the oracle
# asserts concrete parsed values (object names + positions for Blender 2.80/2.90/3.0/3.5 files) —
# a no-op / exit(0) patch cannot pass it.
#
# build.sh pre-compiled everything (`cargo test --no-run` + `cargo build --examples`); this
# script only RUNS the suite. (The doc-test itself is compiled by rustdoc at `cargo test` time
# from the baked registry cache — offline-safe.)
set -uo pipefail
[ -n "${SOURCE_DATE_EPOCH:-}" ] || unset SOURCE_DATE_EPOCH

: "${MAYHEM_JOBS:=$(nproc)}"
cd "$SRC"

# emit_ctrf <tool> <passed> <failed> [skipped] [pending] [other]
emit_ctrf() {
  local tool="$1" passed="$2" failed="$3" skipped="${4:-0}" pending="${5:-0}" other="${6:-0}"
  local tests=$(( passed + failed + skipped + pending + other ))
  cat > "${CTRF_REPORT:-$SRC/ctrf-report.json}" <<JSON
{
  "results": {
    "tool": { "name": "$tool" },
    "summary": {
      "tests": $tests,
      "passed": $passed,
      "failed": $failed,
      "pending": $pending,
      "skipped": $skipped,
      "other": $other
    }
  }
}
JSON
  printf 'CTRF {"results":{"tool":{"name":"%s"},"summary":{"tests":%d,"passed":%d,"failed":%d,"pending":%d,"skipped":%d,"other":%d}}}\n' \
    "$tool" "$tests" "$passed" "$failed" "$pending" "$skipped" "$other"
  [ "$failed" -eq 0 ]
}

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not available — cannot run the test suite" >&2
  emit_ctrf "cargo-test" 0 1 0; exit 2
fi

echo "=== running cargo test (blend lib harness + src/lib.rs doc-test) ==="
# Image-default pinned nightly; RUSTFLAGS cleared so the test build inherits nothing from the
# sanitizer fuzz build (same fingerprint as build.sh's `cargo test --no-run` pre-build).
out="$(RUSTFLAGS="" cargo test --no-fail-fast --jobs "$MAYHEM_JOBS" 2>&1)"; rc=$?
echo "$out"

# libtest prints one line per test binary:
#   test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; ...
# Sum across all binaries (lib harness + doc-tests).
PASSED=0; FAILED=0; IGNORED=0
while read -r p f i; do
  PASSED=$(( PASSED + p )); FAILED=$(( FAILED + f )); IGNORED=$(( IGNORED + i ))
done < <(printf '%s\n' "$out" \
  | sed -n 's/^test result:.* \([0-9][0-9]*\) passed; \([0-9][0-9]*\) failed; \([0-9][0-9]*\) ignored.*/\1 \2 \3/p')

# The suite is exactly one doc-test: if we parsed no result lines, or nothing passed, the run
# itself is broken (e.g. a neutered cargo produces no output) — that is a FAILURE, never a pass.
if [ "$PASSED" -eq 0 ]; then
  echo "cargo test produced no passing tests (parsed passed=$PASSED failed=$FAILED rc=$rc) — failing" >&2
  FAILED=$(( FAILED + 1 ))
fi

# ── Known-answer oracle: upstream example vs committed golden output ───────────────────────────
# target/debug/examples/names_positions (built by build.sh with normal flags) parses the four
# upstream .blend files (2.80/2.90/3.0/3.5) and prints every object's name + location. Its
# output must match mayhem/expected/names_positions.txt byte-for-byte.
BIN=target/debug/examples/names_positions
GOLDEN=mayhem/expected/names_positions.txt
if [ ! -x "$BIN" ]; then
  echo "FAIL - $BIN missing — build.sh must build it (not rebuilding here)" >&2
  FAILED=$(( FAILED + 1 ))
elif CARGO_MANIFEST_DIR="$SRC" "$BIN" > /tmp/names_positions.out 2>/tmp/names_positions.err \
     && diff -u "$GOLDEN" /tmp/names_positions.out; then
  echo "  ok   - names_positions output matches golden ($GOLDEN)"
  PASSED=$(( PASSED + 1 ))
else
  echo "  FAIL - names_positions output does not match golden ($GOLDEN)"
  sed 's/^/        /' /tmp/names_positions.err | tail -5
  FAILED=$(( FAILED + 1 ))
fi

echo "test.sh: passed=$PASSED failed=$FAILED ignored=$IGNORED"
emit_ctrf "cargo-test" "$PASSED" "$FAILED" "$IGNORED"
