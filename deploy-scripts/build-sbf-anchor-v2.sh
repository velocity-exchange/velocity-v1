#!/usr/bin/env bash
# Builds one anchor-v2 program (CLOB, midpoint, ...) at the pinned SBPFv3
# bytecode version, with the same link-time and post-build guards
# deploy-scripts/build-sbf.sh applies to the root-workspace programs.
#
# Usage: build-sbf-anchor-v2.sh <program-dir-name>
set -euo pipefail

PROGRAM="${1:?usage: build-sbf-anchor-v2.sh <program-dir-name>}"

PLATFORM_TOOLS_VERSION="${PLATFORM_TOOLS_VERSION:-v1.57}"
SBPF_ARCH="${SBPF_ARCH:-v3}"

export PATH="$HOME/.cargo/bin:$PATH"
if command -v xcrun >/dev/null 2>&1; then
  export SDKROOT="${SDKROOT:-$(xcrun --show-sdk-path)}"
fi
# Turn an unresolved syscall into a link error, matching build-sbf.sh. Without
# this the linker emits `call -1`, which builds, deploys, and only traps when
# that path runs on chain.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-z -C link-arg=defs"

cd "$(dirname "$0")/../anchor-v2"

cargo-build-sbf \
  --arch "$SBPF_ARCH" \
  --tools-version "$PLATFORM_TOOLS_VERSION" \
  --manifest-path "programs/$PROGRAM/Cargo.toml"

SBPF_ARCH="$SBPF_ARCH" bash "../deploy-scripts/assert-sbpf-version.sh" "target/deploy/$PROGRAM.so"
