# Shared setup for the harnesses that run velocity on a local
# solana-test-validator. Source it from the repo root; it defines functions and
# runs nothing.

# Set SOLANA_TEST_VALIDATOR to the real binary it names, and fail unless it is
# agave 4.2 or later.
#
# `solana-test-validator` on PATH is usually agave's `active_release` symlink,
# and `cargo-build-sbf` moves that symlink when it installs platform-tools. A
# name checked before the build can therefore start a different binary after
# it. That swap is silent and costs a whole run: the version gate passes, an
# older validator starts, and every turner submission is rejected twenty
# minutes later. The caller resolves the name before it builds.
#
# The pinned relay's turner signs transaction v1 (SIMD-0385), and only agave
# 4.2 and later can parse the 0x81 version prefix. An older validator fails
# every turner submission at the RPC after a simulation that looked fine.
resolve_test_validator() {
  # `agave-install` points one global symlink at one release, and a machine
  # that also runs relay's own e2e needs a different one. Naming the binary
  # lets each project have the validator it needs.
  SOLANA_TEST_VALIDATOR="${SOLANA_TEST_VALIDATOR:-solana-test-validator}"
  command -v "$SOLANA_TEST_VALIDATOR" >/dev/null || { echo "$SOLANA_TEST_VALIDATOR not found (set SOLANA_TEST_VALIDATOR)" >&2; exit 1; }
  SOLANA_TEST_VALIDATOR="$(command -v "$SOLANA_TEST_VALIDATOR")"
  while [ -L "$SOLANA_TEST_VALIDATOR" ]; do
    local link_target
    link_target="$(readlink "$SOLANA_TEST_VALIDATOR")"
    case "$link_target" in
      /*) SOLANA_TEST_VALIDATOR="$link_target" ;;
      *) SOLANA_TEST_VALIDATOR="$(dirname "$SOLANA_TEST_VALIDATOR")/$link_target" ;;
    esac
  done

  # `active_release` is a symlinked *directory*, so the loop above does not
  # reach it. `pwd -P` resolves every component.
  SOLANA_TEST_VALIDATOR="$(cd "$(dirname "$SOLANA_TEST_VALIDATOR")" && pwd -P)/$(basename "$SOLANA_TEST_VALIDATOR")"

  local version major minor
  version="$("$SOLANA_TEST_VALIDATOR" --version | awk '{print $2}')"
  major="${version%%.*}"
  minor="${version#*.}"
  minor="${minor%%.*}"
  if [ "$major" -lt 4 ] || { [ "$major" -eq 4 ] && [ "$minor" -lt 2 ]; }; then
    echo "solana-test-validator is $version; relay's crank-turner needs agave 4.2 or later" >&2
    echo "       switch with: agave-install init 4.2.2" >&2
    echo "       SOLANA_TEST_VALIDATOR=<path> names a different binary without moving the global one" >&2
    exit 1
  fi

  echo "== validator $version ($SOLANA_TEST_VALIDATOR) =="
}

# Put relay at the revision velocity builds against in a detached worktree at
# RELAY_SRC, and set RELAY_REV. relay-spec, relay-anchor and
# relay-chain-source all pin the same revision; the program manifest is the
# copy this reads.
#
# A worktree, not a checkout: the relay repository is somebody else's working
# directory, and this must not move it. RELAY_WORKTREE names the directory. It
# is reused between runs to keep the cargo cache warm.
checkout_pinned_relay() {
  RELAY_REPO="${RELAY_REPO:-$HOME/source/relay}"
  RELAY_SRC="${RELAY_WORKTREE:-$HOME/.cache/velocity-e2e/relay}"
  [ -d "$RELAY_REPO" ] || { echo "relay checkout not found at $RELAY_REPO (set RELAY_REPO)" >&2; exit 1; }

  RELAY_REV="$(sed -n 's/^relay-spec = .*rev = "\([0-9a-f]\{7,40\}\)".*/\1/p' programs/velocity/Cargo.toml | head -1)"
  [ -n "$RELAY_REV" ] || { echo "no relay rev found in programs/velocity/Cargo.toml" >&2; exit 1; }
  git -C "$RELAY_REPO" cat-file -e "${RELAY_REV}^{commit}" 2>/dev/null || {
    echo "relay rev $RELAY_REV is not in $RELAY_REPO — run: git -C $RELAY_REPO fetch --all" >&2
    exit 1
  }

  if [ -e "$RELAY_SRC" ]; then
    git -C "$RELAY_SRC" checkout --detach --quiet "$RELAY_REV"
  else
    mkdir -p "$(dirname "$RELAY_SRC")"
    git -C "$RELAY_REPO" worktree add --detach --quiet "$RELAY_SRC" "$RELAY_REV"
  fi

  echo "== relay at $RELAY_REV ($RELAY_SRC) =="
}

# Build the relay program and its crank-turner from RELAY_SRC.
build_relay() {
  (cd "$RELAY_SRC/programs" && cargo-build-sbf --arch v3 --tools-version v1.57 --manifest-path relay/Cargo.toml)
  cargo build --manifest-path "$RELAY_SRC/Cargo.toml" -p relay-crank-turner
}

# Kill whatever holds each port given. A validator orphaned by an earlier
# interrupted run keeps the port and its old ledger, and the next run then
# fails deep inside setup with a confusing "already initialized".
free_ports() {
  local port pids
  for port in "$@"; do
    pids=$(lsof -ti "tcp:$port" 2>/dev/null || true)
    if [ -n "$pids" ]; then
      echo "== port $port busy, killing $pids =="
      kill $pids 2>/dev/null || true
      sleep 2
    fi

    if lsof -ti "tcp:$port" >/dev/null 2>&1; then
      echo "port $port is still held by another process; choose another port" >&2
      exit 1
    fi
  done
}

# Wait up to a minute for the validator on the given port to report healthy.
wait_for_validator() {
  local port="$1" log="$2" i
  echo "== waiting for validator health =="
  for i in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$port" -X POST -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | grep -q '"ok"'; then
      return 0
    fi

    sleep 1
  done

  echo "validator never became healthy — see $log" >&2
  exit 1
}
