#!/usr/bin/env bash
# Regenerates a program's IDL and its TypeScript types.
#
# Usage: build-idl.sh <velocity|vaults>
set -euo pipefail

PROGRAM="${1:?usage: build-idl.sh <velocity|vaults>}"

# `anchor idl build` shells out to `cargo +stable`, which resolves only through
# the rustup shim, so it must come before any other cargo on PATH.
export PATH="$HOME/.cargo/bin:$PATH"

cd "$(dirname "$0")/.."

case "$PROGRAM" in
	velocity)
		# anchor writes neither directory, and cargo-build-sbf does not create
		# them the way `anchor build` used to.
		mkdir -p target/idl target/types
		# The anchor-test flavor on purpose: default features include
		# mainnet-beta, which compiles out the devnet-only instructions that
		# wipe-devnet.ts reaches for through this IDL.
		anchor idl build --skip-lint -p velocity -o target/idl/velocity.json \
			-- --no-default-features --features no-entrypoint,anchor-test
		# sync-idl.sh normalizes the IDL, regenerates the TypeScript mirror from
		# the normalized document, and copies both into packages/sdk.
		bash scripts/sync-idl.sh
		;;
	vaults)
		mkdir -p packages/vaults-sdk/src/idl packages/vaults-sdk/src/types
		anchor idl build --skip-lint -p vaults \
			-o packages/vaults-sdk/src/idl/vaults.json
		anchor idl type packages/vaults-sdk/src/idl/vaults.json \
			--out packages/vaults-sdk/src/types/vaults.ts
		bun run prettify:fix
		;;
	*)
		echo "unknown program: $PROGRAM (want velocity|vaults)" >&2
		exit 1
		;;
esac
