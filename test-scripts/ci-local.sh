#!/usr/bin/env bash
# Local emulation of .github/workflows/main.yml -- run the CI gates before
# pushing, without waiting for (or being able to trigger) the hosted runs.
#
# KEEP THIS FILE IN SYNC WITH .github/workflows/main.yml: when a gating job's
# command changes there, mirror it here in the same change (see CLAUDE.md).
#
# Usage:
#   bash test-scripts/ci-local.sh [tier] [output flags]
#
# Tier (pick one):
#   (none)      cheap tier: fmt, clippy, lint, builds, typechecks, unit tests
#   --full      + anchor/vault integration suites and the rust-workspace test job
#   --fast      static checks only (no test runs)
#
# Output flags:
#   --verbose   stream every check's full output inline (default: one line per
#               check, failure prints the log tail)
#   --no-logs   do not keep log files (default: full output of every check is
#               kept in .ci-local-logs/<check>.log, wiped at each run start)
#
# Not emulated (needs infra CI has and laptops don't): devnet-e2e, rust live
# tests, docker/ECR image builds, the verifiable artifact build, and
# verify-sdk-configs' live-RPC suite (runs only if MAINNET_RPC_ENDPOINT is set).
#
# No fail-fast: every check runs, failures are collected, summary at the end.

set -u
cd "$(dirname "$0")/.."

usage() {
  cat <<USAGE
usage: $0 [tier] [output flags]

Local emulation of the gating jobs in .github/workflows/main.yml.

tier (pick one):
  (none)      fmt, clippy, lint, builds, typechecks, unit tests
  --fast      static checks only (no test runs)
  --full      everything: + anchor/vault integration suites and
              rust-workspace tests (30-40 min, wipes the SBF cache first)

output flags:
  --verbose   stream every check's full output inline
              (default: one line per check, failures print the log tail)
  --no-logs   do not keep per-check log files
              (default: kept in .ci-local-logs/, wiped at each run start)
  -h, --help  this text

not emulated (CI-only infra): devnet-e2e, rust live tests, docker/ECR
builds, verifiable artifact, verify-sdk-configs' live-RPC suite.
USAGE
}

MODE="default"
VERBOSE=0
KEEP_LOGS=1
for arg in "$@"; do
  case "$arg" in
    --full) MODE="full" ;;
    --fast) MODE="fast" ;;
    --verbose) VERBOSE=1 ;;
    --no-logs) KEEP_LOGS=0 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; echo "" >&2; usage >&2; exit 2 ;;
  esac
done

LOG_DIR=".ci-local-logs"
rm -rf "$LOG_DIR"
if [ "$KEEP_LOGS" = "1" ]; then
  mkdir -p "$LOG_DIR"
fi

PASS=()
FAIL=()
SKIP=()

run_check() {
  local name="$1"; shift
  local slug log start elapsed rc
  slug=$(echo "$name" | tr -cs 'a-zA-Z0-9' '-' | sed 's/^-//;s/-$//')
  if [ "$KEEP_LOGS" = "1" ]; then
    log="${LOG_DIR}/${slug}.log"
  else
    log=$(mktemp)
  fi
  start=$SECONDS

  if [ "$VERBOSE" = "1" ]; then
    echo ""
    echo "==> ${name}"
    if [ "$KEEP_LOGS" = "1" ]; then
      "$@" 2>&1 | tee "$log"
      rc=${PIPESTATUS[0]}
    else
      "$@" 2>&1
      rc=$?
    fi
    if [ "$rc" = "0" ]; then PASS+=("$name"); else FAIL+=("$name"); fi
    [ "$KEEP_LOGS" = "1" ] || rm -f "$log"
    return
  fi

  printf '%-55s' "==> ${name}"
  if "$@" > "$log" 2>&1; then
    elapsed=$(( SECONDS - start ))
    echo "ok    (${elapsed}s)"
    PASS+=("$name")
  else
    elapsed=$(( SECONDS - start ))
    echo "FAIL  (${elapsed}s)"
    FAIL+=("$name")
    echo "----- last 30 lines -----"
    tail -30 "$log"
    echo "-------------------------"
  fi
  [ "$KEEP_LOGS" = "1" ] || rm -f "$log"
}

skip_check() {
  SKIP+=("$1 ($2)")
}

# Static checks (all modes). CI: fmt-clippy, prettier, lint.

# fmt:rust:check wraps scripts/fmt-rust.sh: program workspace + rust/ workspace
# + fuzz crates + velocity-rs examples, same nightly-rustfmt style CI enforces
# across its fmt steps.
run_check "rust fmt (all codebases)"  bun run fmt:rust:check
run_check "cargo clippy -p velocity"  cargo clippy -p velocity
run_check "prettier"                  bun run prettify
run_check "eslint"                    bun run lint

# CI job: rust-workspace-check (static parts)
run_check "rust workspace check"      cargo check --manifest-path rust/Cargo.toml --locked --all-targets
run_check "velocity_idl.rs in sync"   git diff --exit-code rust/velocity-rs/crates/src/velocity_idl.rs

# Not a standalone CI job, but the anchor-tests build compiles this flavor;
# checking it here catches feature-gate breakage without an SBF build.
run_check "program check (anchor-test flavor)" \
  cargo check -p velocity --no-default-features --features no-entrypoint,anchor-test

# Builds and typechecks (all modes). CI: ts-build, ts-typecheck, sdk-typecheck.

run_check "TS workspace build"        bunx turbo run build
run_check "TS typecheck"              bunx turbo run typecheck
run_check "SDK strict typecheck"      bash -c "cd packages/sdk && bunx tsc --noEmit -p tsconfig.json"

# Unit tests (default + full). CI: unit-tests, ts-tests, sdk-tests.

if [ "$MODE" != "fast" ]; then
  run_check "cargo check (program workspace)" cargo check
  run_check "cargo test --lib"                cargo test --lib
  run_check "TS unit tests (libs + apps)" \
    bunx turbo run test \
      --filter='./packages/*' --filter='!@velocity-exchange/sdk' \
      --filter=@velocity-exchange/dlob-server \
      --filter=@velocity-exchange/keeper-bots-v2
  run_check "SDK offline suites" bash -c "
    cd packages/sdk &&
    bun run test &&
    bun run test:parity &&
    bun run test:dlob &&
    bun run test:bignum &&
    bun run test:events &&
    bun run test:velocitycore
  "
else
  skip_check "unit tests" "--fast"
fi

# Integration suites (--full only). CI: anchor-tests, vault-tests, router-svm-tests, rust-workspace tests.

if [ "$MODE" = "full" ]; then
  # SBF incremental cache corrupts across feature-flavor switches (e.g. a
  # program:build .so followed by the test build) and produces .so files that
  # die with "Access violation in unknown section" at entry. Always clean
  # before the suite build; CI builds from scratch so it never hits this.
  echo "==> cleaning SBF cache (flavor-poisoning guard)"
  rm -rf target/sbpf*-solana-solana target/deploy

  run_check "router svm tests"         bash -c "
    bash deploy-scripts/build-sbf.sh test protocol-revenue-router &&
    cargo test --manifest-path programs/protocol-revenue-router/svm-tests/Cargo.toml --locked
  "
  run_check "anchor integration suite" bash test-scripts/run-anchor-tests.sh
  run_check "vault tests"              bash -c "
    bash test-scripts/run-vault-tests.sh --build-only &&
    bash test-scripts/run-vault-tests.sh --skip-build
  "
  # CI provides a redis:7 service for this job; the swift crate's
  # confirmation-server tests connect to it. Without a local redis those
  # tests panic on connection, so exclude the crate and say so.
  if redis-cli ping >/dev/null 2>&1; then
    run_check "rust workspace tests" \
      cargo test --manifest-path rust/Cargo.toml --locked --workspace --all-targets
  else
    run_check "rust workspace tests (no redis: swift excluded)" \
      cargo test --manifest-path rust/Cargo.toml --locked --workspace --all-targets --exclude swift-server
    skip_check "swift crate tests" "no local redis (docker run -p 6379:6379 redis:7)"
  fi

  # The anchor suite's own build uses default features (mainnet-beta ON) and
  # syncs its IDL into packages/sdk/src/idl/, a flavor that compiles out
  # devnet-only instructions (e.g. force_wipe_accounts_devnet). Committing
  # that IDL breaks wipe-devnet.ts. Restore the canonical flavor after the
  # suites so a post-run `git add` can't capture the mangled one.
  run_check "restore canonical IDL (program:idl)" bun run program:idl
  run_check "velocity_idl.rs regen"    cargo check --manifest-path rust/Cargo.toml --locked
else
  skip_check "anchor integration suite" "needs --full"
  skip_check "vault tests" "needs --full"
  skip_check "router svm tests" "needs --full"
  skip_check "rust workspace tests" "needs --full"
fi

# Optional. CI: cargo-audit.

if command -v cargo-audit >/dev/null 2>&1; then
  run_check "cargo audit" cargo audit
else
  skip_check "cargo audit" "cargo-audit not installed"
fi

# Optional. CI: cargo-deny. Both workspaces, one shared deny.toml.
#
# CI runs cargo-deny offline against a DB fetched by cargo-audit (its git-CLI
# fetch is rejected unauthenticated by GitHub from the runner); mirror that
# hand-off here so a local run matches what CI actually checks. Unlike CI,
# use a mktemp scratch dir cleaned up by a trap, and never touch the
# developer's own ~/.cargo/advisory-db*.
if command -v cargo-deny >/dev/null 2>&1 && command -v cargo-audit >/dev/null 2>&1; then
  DENY_SCRATCH=$(mktemp -d)
  trap 'rm -rf "$DENY_SCRATCH"' EXIT

  ADVISORY_DB="$DENY_SCRATCH/advisory-db"
  cargo audit --db "$ADVISORY_DB" >/dev/null 2>&1 || true

  if [ -d "$ADVISORY_DB/.git" ]; then
    DENY_DBS="$DENY_SCRATCH/advisory-dbs"
    mkdir -p "$DENY_DBS"
    ln -s "$ADVISORY_DB" "$DENY_DBS/advisory-db-3157b0e258782691"
    export CARGO_DENY_DB_PATH="$DENY_DBS"

    cargo fetch --locked --manifest-path Cargo.toml
    cargo fetch --locked --manifest-path rust/Cargo.toml

    run_check "cargo deny (program workspace)" \
      cargo deny --offline --locked --manifest-path Cargo.toml --config deny.toml check
    run_check "cargo deny (rust workspace)" \
      cargo deny --offline --locked --manifest-path rust/Cargo.toml --config deny.toml check
  else
    skip_check "cargo deny" "offline handoff needs cargo-audit to fetch the advisory database first, and that fetch failed"
  fi
else
  skip_check "cargo deny" "needs both cargo-deny and cargo-audit installed (offline handoff)"
fi

skip_check "verify-sdk-configs" "needs live RPC endpoints"
skip_check "devnet-e2e / rust-live-tests / docker / verified-build" "CI-only infra"

# Summary.

echo ""
echo "============================================"
echo " ci-local summary (${MODE})"
echo "============================================"
for c in "${PASS[@]+"${PASS[@]}"}";  do echo "  ok    $c"; done
for c in "${SKIP[@]+"${SKIP[@]}"}";  do echo "  skip  $c"; done
for c in "${FAIL[@]+"${FAIL[@]}"}";  do echo "  FAIL  $c"; done
echo "============================================"
if [ "$KEEP_LOGS" = "1" ]; then
  echo "full logs: ${LOG_DIR}/"
fi

if [ "${#FAIL[@]}" -gt 0 ]; then
  echo "${#FAIL[@]} check(s) failed"
  exit 1
fi
echo "all checks passed"
