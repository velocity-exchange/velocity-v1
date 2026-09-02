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
#
# READ THE LINE TABLE, NOT .debug_info's DW_AT_name. The SF: path is
# `DW_AT_comp_dir` (when present) + the line program's `include_directories[]`
# entry + `file_names[]`. It is NOT the compile unit's DW_AT_name -- that carries
# a codegen-unit suffix (`.../lib.rs/@/crate.hash-cgu.00`) and, on real SBF
# artifacts, differs from what ends up in the profile. Deriving from DW_AT_name
# looked right, produced `programs/`, and the server rejected it anyway. This is
# the same algorithm bundle-guard.sh GATE E uses; the two must not drift.
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

# THE LINE TABLE IS AUTHORITATIVE, NOT DW_AT_name.
#
# The LCOV `SF:` key the server matches against is
#     DW_AT_comp_dir (when the CU has one) + include_directories[] + file_names[]
# -- NOT the compile unit's DW_AT_name. DW_AT_name carries a codegen-unit suffix
# and, on THIS repo's real artifacts, is repo-RELATIVE while comp_dir is absolute:
#   DW_AT_name     "programs/velocity/src/lib.rs/@/velocity.<hash>-cgu.0"
#   DW_AT_comp_dir "/src"
# So a DW_AT_name-derived check yields "programs/velocity/src/lib.rs", which does
# not start with "/src" -- it REJECTS a perfectly good bundle. That fallback used
# to live here and in a duplicate inside build-bundle.sh; both are gone.
# (Measured on a real container-built symbols .so: 60 CUs, exactly 2 with
# comp_dir "/src" -- velocity and pyth-lazer -- and 58 under the container's
# cargo registry.)
#
# The exclusion regex must catch BOTH "~/.cargo/registry" (host build) and
# "/usr/local/cargo/registry" (the container). The old `\.cargo` alternative
# matched only the first, so every dependency comp_dir survived the filter and
# `head -1` picked whichever CU rustc happened to emit first. It picked the right
# one by luck, not by construction -- a codegen-order change would have silently
# repointed the check at a dependency.
#
# --recurse-depth=0 prints CU-level DIEs only: same comp_dirs, a fraction of the
# work on a ~71 MB .so.
comp="$("$dwdump" --debug-info --recurse-depth=0 "$sym" 2>/dev/null \
  | grep -oE 'DW_AT_comp_dir[[:space:]]*\("[^"]*"\)' \
  | sed -E 's/.*\("([^"]*)"\)/\1/' \
  | grep -vE '(^|/)\.?cargo/registry|/rustc/|toolchain|bpf-tools|platform-tools' \
  | sort -u || true)"

dirs="$("$dwdump" --debug-line "$sym" 2>/dev/null \
  | grep -oE 'include_directories\[[[:space:]]*[0-9]+\][[:space:]]*=[[:space:]]*"[^"]*"' \
  | sed -E 's/.*"([^"]*)"/\1/' | sort -u || true)"

if [ -z "$dirs" ]; then
  echo "verify-coverage: ERROR no line-table directories readable from $sym." >&2
  echo "  Cannot prove SourcesOriginalPath ('$sop') matches the coverage profile." >&2
  echo "  Refusing to ship a bundle whose coverage may render empty." >&2
  exit 1
fi

# Every directory the line program can attribute a line to: absolute entries as
# themselves, relative entries both bare and joined to each first-party comp_dir.
#
# Built through a temp file rather than `keys="$( ... case ... )"`: bash 3.2 (what
# macOS ships, and this script runs on dev boxes as well as CI) mis-parses a
# `case` nested inside a multi-line command substitution -- "syntax error near
# unexpected token `newline'" on the `/*)` pattern.
keys_file="$(mktemp)"
trap 'rm -f "$keys_file"' EXIT
printf '%s\n' "$dirs" | while IFS= read -r d; do
  [ -n "$d" ] || continue
  printf '%s\n' "$d" >> "$keys_file"
  case "$d" in
    /*) ;;
    *)
      printf '%s\n' "$comp" | while IFS= read -r c; do
        [ -n "$c" ] || continue
        printf '%s/%s\n' "${c%/}" "$d" >> "$keys_file"
      done
      ;;
  esac
done
keys="$(sort -u "$keys_file")"

# `|| true` on every count: under `set -o pipefail` a zero-count grep exits 1, the
# assignment fails, and `set -e` kills this script SILENTLY.
pref="${sop%/}/"
hits="$(printf '%s\n' "$keys" | grep -c "^${pref}" || true)"
total="$(printf '%s\n' "$keys" | grep -c . || true)"
if [ "${hits:-0}" -eq 0 ]; then
  echo "verify-coverage: ERROR coverage would render EMPTY." >&2
  echo "  SourcesOriginalPath : $sop" >&2
  echo "  comp_dir(s)         : $(printf '%s' "$comp" | tr '\n' ' ')" >&2
  echo "  It prefixes NONE of the ${total:-0} line-table directories, e.g.:" >&2
  printf '%s\n' "$keys" | head -4 | sed 's/^/    /' >&2
  echo "  Build the coverage .so at a fixed root (container /src or" >&2
  echo "  --remap-path-prefix) and set the manifest to match." >&2
  exit 1
fi
echo "verify-coverage: OK ('$sop' prefixes ${hits}/${total} line-table directories; comp_dir: $(printf '%s' "$comp" | tr '\n' ' '))"
exit 0
