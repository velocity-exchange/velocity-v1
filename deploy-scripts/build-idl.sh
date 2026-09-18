#!/usr/bin/env bash
# Regenerates a program's IDL and its TypeScript types.
#
# Usage: build-idl.sh <velocity|vaults|jit-proxy>
set -euo pipefail

PROGRAM="${1:?usage: build-idl.sh <velocity|vaults|jit-proxy>}"

# `anchor idl build` shells out to `cargo +stable`, which resolves only through
# the rustup shim, so it must come before any other cargo on PATH.
export PATH="$HOME/.cargo/bin:$PATH"

cd "$(dirname "$0")/.."

case "$PROGRAM" in
	velocity)
		# The anchor-test flavor on purpose: default features include
		# mainnet-beta, which compiles out the devnet-only instructions that
		# wipe-devnet.ts reaches for through this IDL.
		anchor idl build --skip-lint -p velocity -o target/idl/velocity.json \
			-- --no-default-features --features no-entrypoint,anchor-test
		cp target/idl/velocity.json packages/sdk/src/idl/velocity.json
		anchor idl type packages/sdk/src/idl/velocity.json \
			--out packages/sdk/src/idl/velocity.ts
		;;
	vaults)
		anchor idl build --skip-lint -p vaults \
			-o packages/vaults-sdk/src/idl/vaults.json
		anchor idl type packages/vaults-sdk/src/idl/vaults.json \
			--out packages/vaults-sdk/src/types/vaults.ts
		bun run prettify:fix
		;;
	jit-proxy)
		anchor idl build --skip-lint -p jit-proxy \
			-o packages/jit-proxy/src/idl/jit_proxy.json
		anchor idl type packages/jit-proxy/src/idl/jit_proxy.json \
			--out packages/jit-proxy/src/types/jit_proxy.ts
		bun run prettify:fix
		;;
	*)
		echo "unknown program: $PROGRAM (want velocity|vaults|jit-proxy)" >&2
		exit 1
		;;
esac
