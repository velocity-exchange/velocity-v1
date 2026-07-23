#!/usr/bin/env bash
# Re-vendor the canonical velocity IDL into each e2e-svm harness.
#
# The e2e-svm* harnesses embed a copy of the program IDL (via
# `crucible_idl_gen::declare_fuzz_program!(velocity_idl = "idls/velocity.json")`)
# because the fuzz/ workspace is separate and cannot read the SDK's generated
# artifact at build time. That copy MUST match the canonical
# `packages/sdk/src/idl/velocity.json`, or the harnesses fuzz a stale ABI.
#
# Run this after any `bun run program:idl` / program change that regenerates the
# IDL. CI (`.github/workflows/fuzz.yml` -> idl-sync) fails if a copy is stale.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
canonical="$repo_root/packages/sdk/src/idl/velocity.json"

if [[ ! -f "$canonical" ]]; then
  echo "canonical IDL not found at $canonical (run 'bun run program:idl' first)" >&2
  exit 1
fi

status=0
for dir in "$repo_root"/fuzz/*/idls; do
  [[ -f "$dir/velocity.json" ]] || continue
  if [[ "${1:-}" == "--check" ]]; then
    if ! diff -q "$canonical" "$dir/velocity.json" >/dev/null 2>&1; then
      echo "STALE: $dir/velocity.json differs from canonical" >&2
      status=1
    fi
  else
    cp "$canonical" "$dir/velocity.json"
    echo "synced $dir/velocity.json"
  fi
done

if [[ "${1:-}" == "--check" && $status -ne 0 ]]; then
  echo "" >&2
  echo "Vendored fuzz IDL(s) are stale. Run: bash fuzz/sync-idls.sh" >&2
  exit 1
fi
