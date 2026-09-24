#!/usr/bin/env bash
# Generate the CLOB and midpoint IDLs the e2e drives their TS clients from.
#
# These programs run on the velocity anchor fork. The fork's `IdlType` derive on
# every wire type (behind each crate's `idl-build-v2` feature) is what lets the
# build reach the types; `anchor idl build` then writes a standard IDL that
# upstream `@anchor-lang/core` reads directly. No post-processing: the fork's
# seed-omit fix (velocity-exchange/anchor, "idl: omit pda metadata when a seed
# cannot be classified") is what lets `-o` reparse its own output.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
out="$here/../tests/e2e/idl"
export SDKROOT="${SDKROOT:-$(xcrun --show-sdk-path)}"
mkdir -p "$out"
cd "$here"
echo "generating clob IDL..."
anchor idl build -p clob -o "$out/clob.json"
echo "generating midpoint IDL..."
anchor idl build -p midpoint -o "$out/midpoint.json"
echo "wrote $out/{clob,midpoint}.json"
