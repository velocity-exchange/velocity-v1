#!/usr/bin/env bash
# Generate the CLOB and midpoint IDLs the e2e drives their TS clients from.
#
# These programs run on the velocity anchor fork (wincode wire), whose IDL emit
# differs from what upstream `@anchor-lang/core` reads: type references carry a
# module path (`state::X`) while the definitions use the bare name, and PDA
# seeds it cannot express come out as `{"kind":"expr"}`. The fork's `IdlType`
# derive on every wire type (behind each crate's `idl-build-v2` feature) is what
# lets the build reach the types at all; this script captures the emitted IDL
# and strips the module paths so the reference and its definition agree. The
# `expr` seeds are left as is — the TS `Program` ignores them, and the e2e
# passes every account explicitly.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
out="$here/../tests/e2e/idl"
export SDKROOT="${SDKROOT:-$(xcrun --show-sdk-path)}"
mkdir -p "$out"
gen() { # program, program-id, out-name
  local prog="$1" addr="$2" name="$3"
  echo "generating $prog IDL..."
  ANCHOR_LOG=true anchor idl build -p "$prog" 2>&1 | \
    python3 "$here/scripts/normalize-idl.py" "$addr" > "$out/$name.json"
  echo "  wrote $out/$name.json"
}
cd "$here"
gen clob     BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU clob
gen midpoint eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D  midpoint
