#!/bin/sh
# Builds the deployable devnet artifacts: velocity with the post-audit features
# live (mainnet builds compile them out) plus the token_faucet used to hand out
# devnet USDT. The feature flags, the SBPF bytecode version and the platform-
# tools version all live in build-sbf.sh.
set -eu
cd "$(dirname "$0")/.."
bash deploy-scripts/build-sbf.sh devnet velocity token_faucet
