#!/usr/bin/env bash
#
# sync-idl.sh — normalize the freshly built velocity IDL and copy it into the SDK.
#
# Anchor builds a type reference from the tokens a field is written with, but it
# names the type with `IdlBuild::get_full_path()`. A host that monomorphizes its
# own name therefore emits a reference that carries generic arguments for a type
# definition that declares no generic parameters. `relay_anchor::RelayBlock<C, R>`
# is one: its definition is named `RelayBlock<C>x<R>` and declares no generics,
# while every field spelled `RelayBlock<2, 8>` references it with two const args.
#
# A consumer that generates code from the IDL then writes a generic use of a
# non-generic type, which does not compile. The pass below drops generic
# arguments from any reference whose definition declares none; a reference to a
# genuinely generic type is left alone. Run before `anchor idl type`, so the
# TypeScript mirror carries the same shape.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
idl="$repo_root/target/idl/velocity.json"
types="$repo_root/target/types/velocity.ts"

[ -f "$idl" ] || { echo "sync-idl: $idl not found; build the IDL first" >&2; exit 1; }

python3 - "$idl" <<'PY'
import json, sys

path = sys.argv[1]
with open(path) as f:
    idl = json.load(f)

non_generic = {
    t["name"] for t in idl.get("types", []) if not t.get("generics")
}

stripped = set()

def walk(node):
    if isinstance(node, dict):
        defined = node.get("defined")
        if (
            isinstance(defined, dict)
            and defined.get("generics")
            and defined.get("name") in non_generic
        ):
            stripped.add(defined["name"])
            del defined["generics"]
        for value in node.values():
            walk(value)
    elif isinstance(node, list):
        for item in node:
            walk(item)

walk(idl)

# Match what anchor writes: two-space indent, UTF-8 kept as characters.
with open(path, "w", encoding="utf-8") as f:
    json.dump(idl, f, indent=2, ensure_ascii=False)
    f.write("\n")

if stripped:
    print("sync-idl: dropped generic arguments on " + ", ".join(sorted(stripped)))
PY

# The TypeScript mirror is the same document, so regenerate it from the
# normalized JSON rather than copying whatever the build left behind.
anchor idl type "$idl" --out "$types"

cp "$idl" "$repo_root/packages/sdk/src/idl/velocity.json"
cp "$types" "$repo_root/packages/sdk/src/idl/velocity.ts"
