#!/usr/bin/env bash
# verify-coverage.sh <bundle-dir>
#
# Prove that the bundle's SourcesOriginalPath actually matches the DWARF in its
# symbols .so, BEFORE the bundle is uploaded.
#
# This runs on the HOST, deliberately. build-bundle.sh does its compiling inside a
# linux/amd64 container, but that image ships no llvm-dwarfdump, runs as a non-root
# user so it cannot install one, and a host llvm bind-mounted into it does not
# execute (Ubuntu-noble binaries against a Debian-bookworm runtime) -- it just
# returns EMPTY output. That made the in-container version of this check report
# "no compile-unit paths readable" for a .so that was in fact perfect: 60 compile
# units, DW_AT_comp_dir "/src". Verifying on the host removes that whole class of
# false negative.
#
# Why it matters: a wrong prefix does not error the campaign. The cover task fails
# with "SourcesOriginalPath ... does not match any source file", the dashboard shows
# lines_found: 0, and every CI step stays green. This is the only thing between that
# and a silent loss of source-level coverage.
set -euo pipefail

bundle="${1:?usage: verify-coverage.sh <bundle-dir>}"
manifest="$bundle/manifest.fc.json"
[ -f "$manifest" ] || { echo "verify-coverage: no manifest at $manifest" >&2; exit 1; }

dwdump="$(command -v llvm-dwarfdump || command -v dwarfdump || true)"
if [ -z "$dwdump" ]; then
  # Debian/Ubuntu ship a VERSIONED binary with no unsuffixed alias, so `command -v`
  # finds nothing on a runner that did install llvm.
  for c in /usr/lib/llvm-*/bin/llvm-dwarfdump /usr/bin/llvm-dwarfdump-*; do
    [ -x "$c" ] && dwdump="$c" && break
  done
fi
if [ -z "$dwdump" ]; then
  echo "verify-coverage: ERROR no llvm-dwarfdump available; cannot prove the coverage" >&2
  echo "                 prefix. Install llvm and re-run -- skipping this check is how" >&2
  echo "                 a bundle ships that renders no coverage at all." >&2
  exit 1
fi

sop="$(grep -oE '"SourcesOriginalPath": *"[^"]*"' "$manifest" | head -1 \
       | sed -E 's/.*"SourcesOriginalPath": *"([^"]*)".*/\1/' || true)"
sym_rel="$(grep -oE '"SymbolsPathInBundle": *"[^"]*"' "$manifest" | head -1 \
       | sed -E 's/.*"SymbolsPathInBundle": *"([^"]*)".*/\1/' || true)"

if [ -z "$sop" ] || [ -z "$sym_rel" ]; then
  echo "verify-coverage: no coverage keys in the manifest; nothing to verify." >&2
  exit 0
fi
sym="$bundle/$sym_rel"
[ -f "$sym" ] || { echo "verify-coverage: ERROR symbols missing at $sym" >&2; exit 1; }

# The program crate's own comp_dir, ignoring dependency/toolchain units.
compdir="$("$dwdump" --debug-info "$sym" 2>/dev/null \
  | grep -oE 'DW_AT_comp_dir[^"]*"[^"]+"' | sed -E 's/.*"([^"]+)".*/\1/' \
  | grep -viE '\.cargo|/rustc|/toolchains|platform-tools|bpf-tools' | head -1 || true)"

if [ -n "$compdir" ]; then
  case "$compdir" in "$sop"*) ok=1 ;; *) case "$sop" in "$compdir"*) ok=1 ;; *) ok=0 ;; esac ;; esac
  if [ "$ok" -ne 1 ]; then
    echo "verify-coverage: ERROR coverage would render EMPTY." >&2
    echo "  SourcesOriginalPath : $sop" >&2
    echo "  DWARF comp_dir      : $compdir" >&2
    echo "  These do not share a root. Build the coverage .so at a fixed root" >&2
    echo "  (container /src or --remap-path-prefix) and set the manifest to match." >&2
    exit 1
  fi
  echo "verify-coverage: OK (SourcesOriginalPath '$sop' consistent with comp_dir '$compdir')"
  exit 0
fi

# No comp_dir: SBF DWARF frequently omits it. Fall back to the authoritative test --
# does the prefix actually prefix a compile-unit path?
units="$("$dwdump" --debug-info "$sym" 2>/dev/null \
  | grep -oE 'DW_AT_name[[:space:]]*\("[^"]*\.rs[^"]*"\)' \
  | sed -E 's/.*\("([^"]*)"\)/\1/; s#/@/.*##' | sort -u || true)"
if [ -z "$units" ]; then
  echo "verify-coverage: ERROR no compile-unit paths readable from $sym." >&2
  echo "  Cannot prove SourcesOriginalPath ('$sop') matches the coverage profile." >&2
  exit 1
fi
if printf '%s\n' "$units" | grep -q "^${sop%/}/"; then
  echo "verify-coverage: OK (no comp_dir; '$sop' prefixes $(printf '%s\n' "$units" | grep -c "^${sop%/}/") compile-unit paths)"
  exit 0
fi
echo "verify-coverage: ERROR coverage would render EMPTY." >&2
echo "  SourcesOriginalPath ('$sop') prefixes NONE of the $(printf '%s\n' "$units" | wc -l | tr -d ' ') compile-unit paths, e.g.:" >&2
printf '%s\n' "$units" | head -3 | sed 's/^/    /' >&2
exit 1
