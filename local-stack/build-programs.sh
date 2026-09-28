#!/usr/bin/env bash
# Build the SBF programs the local stack's validator loads. They build on the
# host because the SBF toolchain is a host install. See local-stack/README.md.
set -euo pipefail
cd "$(dirname "$0")/.."
source test-scripts/_localnet.sh

checkout_pinned_relay
bash deploy-scripts/build-sbf.sh devnet velocity
bun run program:build:clob
bun run program:build:midpoint
build_relay
