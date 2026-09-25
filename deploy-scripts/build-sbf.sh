#!/usr/bin/env bash
# Builds the SBF programs at the pinned bytecode version and platform-tools.
#
# Calls cargo-build-sbf directly, which is what Anza's SBPFv3 guide specifies.
# `anchor build` cannot emit v3: it passes its own --tools-version, pinned to a
# platform-tools with no sbpfv3 sysroot, and rejects an override as a duplicate
# argument. The IDL comes from `anchor idl build` (see build-idl.sh).
#
# Usage: build-sbf.sh <test|devnet|mainnet> [program ...]
# Programs outside the flavor default (e.g. protocol-revenue-router) are built by name.
set -euo pipefail

FLAVOR="${1:?usage: build-sbf.sh <test|devnet|mainnet> [program ...]}"
shift || true

# Minimum for SBPFv3 is platform-tools v1.56 / cargo-build-sbf 4.2.0. Never pass
# --arch v3 to platform-tools older than v1.53: it emits bytecode that loads and
# then misbehaves rather than failing the build.
PLATFORM_TOOLS_VERSION="${PLATFORM_TOOLS_VERSION:-v1.57}"
SBPF_ARCH="${SBPF_ARCH:-v3}"
OUT_DIR="${SBF_OUT_DIR:-target/deploy}"

# `cargo +toolchain` resolves only through the rustup shim, so it must come
# before any other cargo on PATH.
export PATH="$HOME/.cargo/bin:$PATH"
# The Solana clang carries no macOS SDK path of its own.
if command -v xcrun >/dev/null 2>&1; then
  export SDKROOT="${SDKROOT:-$(xcrun --show-sdk-path)}"
fi
# Turn an unresolved syscall into a link error. Without this the linker emits
# `call -1`, which builds, deploys, and only traps when that path runs on chain.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-z -C link-arg=defs"

case "$FLAVOR" in
  test)    VELOCITY_ARGS=(--no-default-features --features no-entrypoint,anchor-test); ROUTER_ARGS=(--features anchor-test) ;;
  devnet)  VELOCITY_ARGS=(--no-default-features --features no-entrypoint,isolated-position,vlp-hedge); ROUTER_ARGS=(--no-default-features) ;;
  mainnet) VELOCITY_ARGS=(); ROUTER_ARGS=() ;;
  *) echo "unknown flavor: $FLAVOR (want test|devnet|mainnet)" >&2; exit 1 ;;
esac

build_one() {
  local name="$1"; shift
  echo "==> $name ($FLAVOR, sbpf $SBPF_ARCH, platform-tools $PLATFORM_TOOLS_VERSION)"
  cargo-build-sbf \
    --arch "$SBPF_ARCH" \
    --tools-version "$PLATFORM_TOOLS_VERSION" \
    --manifest-path "programs/$name/Cargo.toml" \
    --sbf-out-dir "$OUT_DIR" \
    -- "$@"
}

PROGRAMS=("$@")
if [ ${#PROGRAMS[@]} -eq 0 ]; then
  case "$FLAVOR" in
    mainnet) PROGRAMS=(velocity) ;;
    devnet)  PROGRAMS=(velocity token_faucet) ;;
    # The integration suites load all four Anchor.toml localnet programs, plus
    # jit-proxy for tests/velocity/jitProxy.ts. Each needs its own feature flags:
    # applying velocity's --no-default-features to the others strips their
    # entrypoints and produces 896-byte stubs.
    test)    PROGRAMS=(velocity vaults jit-proxy pyth token_faucet) ;;
  esac
fi

mkdir -p "$OUT_DIR"
for p in "${PROGRAMS[@]}"; do
  case "$p" in
    velocity)     build_one velocity "${VELOCITY_ARGS[@]}" ;;
    vaults)       build_one vaults --features anchor-test ;;
    jit-proxy)    build_one jit-proxy ;;
    pyth)         build_one pyth ;;
    token_faucet) build_one token_faucet ;;
    # Default features keep the mainnet init gate on; test and devnet drop it.
    protocol-revenue-router) build_one protocol-revenue-router "${ROUTER_ARGS[@]}" ;;
    *)            build_one "$p" ;;
  esac
done

# jit-proxy's crate is `jit-proxy` but the .so Anchor.toml and the tests expect
# is jit_proxy.so; cargo already emits the underscored name, so nothing to do.
echo "SBF artifacts in $OUT_DIR:"
ls -la "$OUT_DIR"/*.so

# Anza migration step 4: verify the emitted bytecode is the version we asked for.
SBPF_ARCH="$SBPF_ARCH" bash "$(dirname "$0")/assert-sbpf-version.sh" "$OUT_DIR"/*.so
