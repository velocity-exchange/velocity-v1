#!/usr/bin/env bash
#
# build-bundle.sh — assemble an AR fuzzing bundle from the velocity Crucible harnesses.
#
# An AR fuzzing bundle is a directory (zipped for upload) laid out as:
#
#   manifest.fc.json                 v3 manifest, one lineage per discovery target
#   bin/<crate>/<feature>            the compiled crucible harness binary
#   target/deploy/velocity.so        the devnet program .so (SVM-tier harnesses only)
#
# The crucible driver runs each binary directly, selecting its mode from FUZZ_*
# env vars (see AR fuzzing worker/drivers/crucible). SVM-tier harnesses open
# `../../target/deploy/velocity.so` relative to their CWD, so their config sets
# HarnessRunDirInBundle = bin/<crate>, from which `../../target/deploy` resolves
# to the bundle-root target/deploy.
#
# The set of targets lives in bundle/targets.txt (the single source of truth).
# The committed manifest.fc.json is generated from it with a placeholder commit;
# `--check` fails if they drift (CI contract, same idea as sync-idls.sh).
#
# Usage:
#   bash fuzz/build-bundle.sh --check          # verify committed manifest is in sync
#   bash fuzz/build-bundle.sh                  # build all binaries + stage bundle dir
#   bash fuzz/build-bundle.sh --zigbuild       # cross-compile to linux/amd64
#   bash fuzz/build-bundle.sh --zip            # also produce dist/velocity-fuzz-bundle.zip
#   bash fuzz/build-bundle.sh --skip-build     # (re)stage + manifest only, reuse binaries
#
# Options:
#   --check         Regenerate the manifest from targets.txt and diff against the
#                   committed fuzz/manifest.fc.json. No compilation. Exit 1 on drift.
#   --zigbuild      Cross-compile the SVM harnesses to x86_64-unknown-linux-gnu
#                   with `cargo zigbuild`, instead of building natively in a
#                   container. This is the ONLY way to produce cloud-arch
#                   (linux/amd64) binaries on an Apple Silicon host: rustc runs
#                   natively on arm64 and merely TARGETS amd64, so it never hits
#                   the QEMU rustc segfault that emulating an amd64 toolchain
#                   does (reproduced here on a hello-world: `qemu: uncaught
#                   target signal 11`). Implies --native — no container is used.
#                   Requires: cargo-zigbuild, zig, and
#                   `rustup target add x86_64-unknown-linux-gnu` for the
#                   toolchain in fuzz/rust-toolchain.toml (NOT just the default
#                   toolchain — missing it fails with "can't find crate for core").
#   --skip-build    Do not run cargo; assume each fuzz/<crate>/target/release binary
#                   already exists. Still stages + regenerates the manifest.
#   --zip           After staging, zip the bundle to dist/velocity-fuzz-bundle.zip.
#   --out DIR       Bundle staging dir (default: fuzz/dist/bundle).
#   --commit SHA    Revision.Commit to embed (default: git HEAD of the repo).
#   --docker        Force the linux/amd64 container cross-build (see below).
#   --native        Force a native build (skip the container even on macOS).
#   --image IMG     Container image for the cross-build (default rust:<toolchain>-bookworm).
#   -h | --help     Show this help.
#
# Cross-build: AR fuzzing workers are linux/amd64, so the bundle binaries must be
# linux/amd64 ELF. On a linux/amd64 host this script builds natively. On any
# other host (e.g. macOS/arm64) it AUTOMATICALLY re-runs the build inside a
# --platform linux/amd64 container (docker, or podman) so the artifacts are
# correct — Docker Desktop uses Rosetta for fast amd64 emulation. Override with
# --native (build for the host arch; not deployable) or --docker (force it).

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/.." && pwd)"
# Target list — override with FUZZ_TARGETS_FILE (must be inside the repo) to build
# a subset, e.g. a quick single-crate smoke bundle. --check/--write-manifest always
# use the canonical list so the committed manifest never reflects a subset.
targets_file="${FUZZ_TARGETS_FILE:-$here/bundle/targets.txt}"
canonical_targets="$here/bundle/targets.txt"
committed_manifest="$here/manifest.fc.json"

# Placeholder commit baked into the committed manifest (valid per AR fuzzing's
# Revision.Validate: 5–40 hex chars). Overwritten with the real HEAD at build time.
placeholder_commit="0000000"

out_dir="$here/dist/bundle"
mode_check=0
mode_write_manifest=0
no_svm=0
skip_build=0
do_zip=0
commit=""
force_docker=0
force_native=0
image=""
svm_only=0
zigbuild=0

while [ $# -gt 0 ]; do
  case "$1" in
    --check) mode_check=1 ;;
    --write-manifest) mode_write_manifest=1 ;;
    --no-svm | --host-only) no_svm=1 ;;   # --host-only: deprecated alias
    --svm-only) svm_only=1 ;;
    --zigbuild) zigbuild=1; force_native=1 ;;
    --skip-build) skip_build=1 ;;
    --zip) do_zip=1 ;;
    --out) out_dir="$2"; shift ;;
    --commit) commit="$2"; shift ;;
    --docker) force_docker=1 ;;
    --native) force_native=1 ;;
    --image) image="$2"; shift ;;
    -h|--help) sed -n '2,49p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

if [ "$zigbuild" -eq 1 ]; then
  command -v cargo-zigbuild >/dev/null 2>&1 || { echo "--zigbuild needs cargo-zigbuild (cargo install cargo-zigbuild)" >&2; exit 2; }
  command -v zig >/dev/null 2>&1 || { echo "--zigbuild needs zig on PATH" >&2; exit 2; }
fi

if [ "$no_svm" -eq 1 ] && [ "$svm_only" -eq 1 ]; then
  echo "--no-svm and --svm-only are mutually exclusive" >&2
  exit 2
fi

[ -f "$targets_file" ] || { echo "missing $targets_file" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "python3 is required" >&2; exit 1; }

# gen_manifest <commit> <no_svm> [svm_only] — emit manifest JSON to stdout from targets.txt.
gen_manifest() {
  python3 - "$targets_file" "$1" "$2" "${3:-0}" <<'PY'
import json, sys

targets_path, commit = sys.argv[1], sys.argv[2]
no_svm = sys.argv[3] == "1"
# A subset bundle must declare ONLY the targets it actually stages: a manifest
# naming a binary the bundle does not contain fails validation upload-side.
svm_only = len(sys.argv) > 4 and sys.argv[4] == "1"

# Per-tier resource footprint. Kept modest for the initial allocation; the
# fuzzing server schedules within the project's compute budget regardless.
# stall/yield (minutes) also tune how fast a lineage cycles: an explore task
# completes when it stalls (no new coverage for `stall`) or yields (`yield`), and
# LineageCover (→ FCOV) is only scheduled after enough completed explores. All
# tiers use a 2h yield: longer uninterrupted explore cycles per lineage, trading
# coverage/FCOV latency (the coverage tiers now cadence every ~2h instead of
# ~20min) for more fuzzing done per cycle. Constraints:
# yield ∈ [20,10080], stall ∈ [0,300], yield ≥ stall, yield-stall ≥ 5.
# One tier: crucible runs the on-chain (svm) targets. Coverage comes from the
# JIT-instrumented SVM executing velocity.so — see targets.txt.
# 4 GiB, not 2: explore fits in 2 GiB but corpus_merge does not. cmin holds the whole
# corpus and was being SIGKILLed, surfacing as `container status code: 137` on the
# corpus_merge task -- which reads like a harness crash. Coverage keeps rendering
# throughout, so the only real symptom is a corpus that quietly stops consolidating.
# MemoryKiB applies to every task type for the lineage, so it is raised here.
# Cores is 8, not 1, and that is load-bearing: it is what leaves the scheduler any
# headroom at all. The orchestrator fills the org's core budget with explore tasks, so
# 1-core tasks tile a budget exactly and leave zero remainder -- corpus_merge and
# project_cover then never get a core and simply never run. That is not a slow cadence;
# they stay queued forever, so the project-level coverage the dashboard renders never
# appears (per-lineage coverage still does, because it is scheduled before explore ramps).
# Measured on lumen/velocity-v1 (118-core budget): at Cores=1 explore held 118/118 and
# project_cover never ran across two bundles; at Cores=8 explore tiles to 14 x 8 = 112,
# the leftover 6 cores are too few for a 15th explore task, and corpus_merge +
# project_cover both ran within two minutes.
# NOTE: this depends on `cores` not dividing the budget evenly. 8 leaves a remainder
# against 118; if the org budget changes to a multiple of 8, pick another value.
TIER = {
    "svm":  {"mem_kib": 4 * 1024 * 1024, "cores": 8, "stall": 0,  "yield": 120},  # 4 GiB, LiteSVM on-chain; 2h explore cycle
}

lineages = []
with open(targets_path) as f:
    for raw in f:
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) not in (3, 4):
            sys.exit(f"malformed target line: {raw!r}")
        # Optional 4th field: a corpus-generation suffix appended to the lineage name.
        # Lineage identity is the name, and the server keys the accumulated corpus to it,
        # so bumping this deliberately abandons a corpus and starts the lineage clean.
        # Needed when a corpus grows past what cmin can consolidate and every later merge
        # inherits the same backlog. Leave it off unless you mean to discard the corpus.
        tier, crate, feature = parts[0], parts[1], parts[2]
        lineage_suffix = parts[3] if len(parts) == 4 else ""
        if tier not in TIER:
            sys.exit(f"unknown tier {tier!r} in: {raw!r}")
        if no_svm and tier == "svm":
            continue
        if svm_only and tier == "lf":
            continue

        params = {"BinaryPathInBundle": f"bin/{crate}/{feature}"}
        driver_type = "crucible"
        if tier == "svm":
            # CWD from which the harness's hardcoded ../../target/deploy/velocity.so resolves.
            params["HarnessRunDirInBundle"] = f"bin/{crate}"
            # Crucible collects coverage from the JIT-instrumented SVM executing velocity.so.
            # This is exactly why crucible now runs ONLY on-chain targets: a crucible harness
            # that calls Rust directly (no program) produces zero edges by design. Sources +
            # SourcesOriginalPath enable the LineageCover task (the driver gates it on both);
            # Symbols points at the program .so whose DWARF gives source-level (vs PC-keyed) LCOV.
            # The crucible LCOV keys each line on an ABSOLUTE SF: path = DW_AT_comp_dir + relative
            # file (see crucible docs/coverage.md). `cargo-build-sbf --debug` sets comp_dir; our
            # containerized build runs at `/src` (repo mounted there), so the paths are
            # `/src/programs/velocity/src/...`. SourcesOriginalPath is that comp_dir prefix `/src`;
            # the driver strips it and looks the remainder up under srcs/ (staged repo-root below).
            # CRITICAL: the .so MUST be the container build (comp_dir=/src). A local/CI-host build
            # bakes an absolute machine path (e.g. /Users/.../velocity-v1 or /home/runner/...) into
            # the DWARF, which /src does not prefix -> the cover task fails "SourcesOriginalPath ...
            # does not match any source file" and coverage comes back empty. Never ship a
            # pre-staged host-built .so for coverage.
            params["SourcesPathInBundle"] = "srcs"
            params["SourcesOriginalPath"] = "/src"
            # NOTE: must NOT contain "target/" — crucible's build_dwarf_source_map infers a
            # source root by splitting FUZZ_SYMBOLS at "/target/", which corrupts DWARF path
            # resolution (→ 0 source files → ToFCov fails → no FCOV). Stage it under symbols/.
            params["SymbolsPathInBundle"] = "symbols/velocity.debug.so"

        lineages.append({
            "Name": f"{crate}__{feature}{lineage_suffix}",
            "Env": {},
            "Confs": [{
                "Name": "explore",
                "Driver": {"Type": driver_type, "Params": params},
                "Architecture": {"Name": "amd64", "Extensions": []},
                "MemoryKiB": TIER[tier]["mem_kib"],
                "Cores": TIER[tier]["cores"],
                "StallTimeMinutes": TIER[tier]["stall"],
                "YieldTimeMinutes": TIER[tier]["yield"],
            }],
        })

manifest = {
    "Version": 3,
    "Revision": {"Commit": commit, "Checkouts": {}},
    "Lineages": lineages,
}
print(json.dumps(manifest, indent=2))
PY
}

if [ "$mode_check" -eq 1 ]; then
  targets_file="$canonical_targets"   # committed manifest reflects the full list, never a subset
  if [ ! -f "$committed_manifest" ]; then
    echo "FAIL: $committed_manifest does not exist; run: bash fuzz/build-bundle.sh --check --write" >&2
    exit 1
  fi
  tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT
  gen_manifest "$placeholder_commit" 0 > "$tmp"
  if ! diff -u "$committed_manifest" "$tmp"; then
    echo "" >&2
    echo "FAIL: fuzz/manifest.fc.json is out of sync with fuzz/bundle/targets.txt." >&2
    echo "Regenerate with: bash fuzz/build-bundle.sh --write-manifest" >&2
    exit 1
  fi
  echo "OK: manifest.fc.json in sync with targets.txt"
  exit 0
fi

# Regenerate the committed manifest (placeholder commit) from targets.txt and exit.
if [ "$mode_write_manifest" -eq 1 ]; then
  targets_file="$canonical_targets"   # never write a subset into the committed manifest
  gen_manifest "$placeholder_commit" 0 > "$committed_manifest"
  echo "wrote $committed_manifest"
  exit 0
fi

# ----------------------------------------------------------------------------
# Build mode.
# ----------------------------------------------------------------------------
[ -n "$commit" ] || commit="$(git -C "$repo_root" rev-parse HEAD)"

host_os="$(uname -s)"; host_arch="$(uname -m)"
in_container="${FUZZ_BUNDLE_IN_CONTAINER:-0}"
is_linux_amd64=0
if [ "$host_os" = "Linux" ] && { [ "$host_arch" = "x86_64" ] || [ "$host_arch" = "amd64" ]; }; then
  is_linux_amd64=1
fi

# The build ALWAYS runs inside --platform linux/amd64 containers (the default) so
# the invoking host OS is irrelevant: harness binaries come out as deployable ELF,
# and the SVM-tier velocity.so builds with the correct Solana toolchain (which is
# broken on macOS). Two images: the SBF-builder (for velocity.so)
# and the rust toolchain image (for the harness binaries + staging). Pass --native
# to build directly on the host instead. Only the outer invocation dispatches; the
# inner run sets FUZZ_BUNDLE_IN_CONTAINER=1 + --native and builds for real.
if [ "$skip_build" -eq 0 ] && [ "$in_container" != "1" ] && [ "$force_native" -eq 0 ]; then
  runtime=""
  if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    runtime=docker
  elif command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
    runtime=podman
  else
    echo "ERROR: the containerized build needs a running docker or podman daemon." >&2
    echo "       Start Docker Desktop (or 'podman machine start'), then retry; or pass" >&2
    echo "       --native to build on the host directly (needs the full local toolchain)." >&2
    exit 1
  fi

  # (1) SVM tier: build velocity.so in the pinned Solana verifiable-build image
  # (skip if already present). The SBF toolchain cargo-build-sbf needs is only
  # correct inside this image — the host/default platform-tools (v1.48, cargo 1.84)
  # is too old for the program's deps (edition2024) and Anchor 1.0 (rustc >= 1.89).
  # This is exactly the image CI's `solana-verify build` uses.
  has_svm_bundle=0
  if [ "$no_svm" -eq 0 ] && grep -qE '^[[:space:]]*svm[[:space:]]' "$targets_file"; then
    has_svm_bundle=1
  fi
  if [ "$has_svm_bundle" -eq 1 ] && [ ! -f "$repo_root/target/deploy/velocity.so" ]; then
    sbf_img="${sbf_image:-solanafoundation/solana-verifiable-build:3.1.14}"
    echo ">> building velocity.so (+ DWARF symbols) in $runtime ($sbf_img)"
    mkdir -p "$repo_root/target/deploy"
    # Two artifacts from ONE build (PC addresses must match): the stripped deploy .so
    # for SVM execution, and the unstripped DWARF .so used as coverage symbols. The
    # profile env (opt-level=1, debug=2, strip=false) is what crucible's docs/coverage.md
    # requires for source-level LCOV — without DWARF the harness emits PC-keyed LCOV
    # (SF: program_<hash>.bpf), and AR fuzzing's ToFCov then FAILS (SourcesOriginalPath
    # matches no source file) => no server-side FCOV. v1.52 platform-tools (this image)
    # is >= v1.51, avoiding the SBF-linker DWARF-corruption bug. Named volume caches the
    # build; the image runs as root so we chown the outputs back to the host user.
    "$runtime" run --rm --platform linux/amd64 \
      -v "$repo_root":/src \
      -v velocity-sbf-cache:/src/programs/velocity/target \
      -v velocity-sbf-cargo:/root/.cargo \
      -w /src/programs/velocity \
      -e CARGO_PROFILE_RELEASE_OPT_LEVEL=1 \
      -e CARGO_PROFILE_RELEASE_DEBUG=2 \
      -e CARGO_PROFILE_RELEASE_STRIP=false \
      "$sbf_img" bash -c "set -e
        cargo-build-sbf --debug --sbf-out-dir /src/target/deploy -- --no-default-features --features no-entrypoint,isolated-position,vlp-hedge
        # The unstripped DWARF .so lands in the cargo target dir. velocity is a workspace member,
        # so cargo uses the WORKSPACE target (/src/target/...), not the crate-local one — take
        # whichever exists.
        dbg=\$(ls /src/target/sbpf-solana-solana/release/velocity.so /src/target/sbf-solana-solana/release/velocity.so /src/programs/velocity/target/sbpf-solana-solana/release/velocity.so /src/programs/velocity/target/sbf-solana-solana/release/velocity.so 2>/dev/null | head -1)
        [ -n \"\$dbg\" ] || { echo 'unstripped velocity.so not found in any target dir' >&2; exit 1; }
        cp \"\$dbg\" /src/target/deploy/velocity.debug.so
        chown $(id -u):$(id -g) /src/target/deploy/velocity.so /src/target/deploy/velocity.debug.so" >&2
    [ -f "$repo_root/target/deploy/velocity.so" ] && [ -f "$repo_root/target/deploy/velocity.debug.so" ] \
      || { echo "ERROR: velocity.so build failed" >&2; exit 1; }
    echo "   built target/deploy/velocity.so (+ velocity.debug.so, DWARF symbols)"
  fi

  # (2) Harness binaries + staging in the rust toolchain image.
  toolchain="$(sed -nE 's/^channel = "([^"]+)".*/\1/p' "$here/rust-toolchain.toml" 2>/dev/null | head -1)"
  img="${image:-rust:${toolchain:-1.91.1}-bookworm}"
  # Map an optional subset targets file (must be inside the repo) to its /src path.
  container_tf="/src/fuzz/bundle/targets.txt"
  if [ -n "${FUZZ_TARGETS_FILE:-}" ]; then
    abs_tf="$(cd "$(dirname "$FUZZ_TARGETS_FILE")" && pwd)/$(basename "$FUZZ_TARGETS_FILE")"
    case "$abs_tf" in
      "$repo_root"/*) container_tf="/src/${abs_tf#"$repo_root"/}" ;;
      *) echo "ERROR: FUZZ_TARGETS_FILE must be inside the repo for the container build" >&2; exit 1 ;;
    esac
  fi
  inner=(--commit "$commit" --native)
  [ "$no_svm" -eq 1 ] && inner+=(--no-svm)
  [ "$svm_only" -eq 1 ] && inner+=(--svm-only)
  [ "$zigbuild" -eq 1 ] && inner+=(--zigbuild)
  [ "$do_zip" -eq 1 ] && inner+=(--zip)
  # The coverage-prefix gate runs in the INNER (containerized) invocation, but the
  # rust image ships no llvm-dwarfdump and the container runs as a non-root user, so
  # it cannot apt-get one. Installing llvm on the runner is therefore not enough --
  # the gate aborted the whole build with "no llvm-dwarfdump/dwarfdump available"
  # AFTER staging everything. Bind-mount the host's llvm tree read-only and put it
  # on PATH so the gate can actually run where it lives.
  echo ">> building harnesses in $runtime (image $img, --platform linux/amd64)"
  # NOT `exec`: the coverage gate is deferred out of the container (see the gate),
  # so the host must regain control afterwards to run it against the staged bundle.
  "$runtime" run --rm --platform linux/amd64 \
    --user "$(id -u):$(id -g)" \
    -v "$repo_root":/src -w /src \
    -e HOME=/tmp \
    -e CARGO_HOME=/src/fuzz/dist/.cargo \
    -e FUZZ_BUNDLE_IN_CONTAINER=1 \
    -e "FUZZ_TARGETS_FILE=$container_tf" \
    "$img" bash fuzz/build-bundle.sh "${inner[@]}"
  rc=$?
  [ "$rc" -ne 0 ] && exit "$rc"
  # Verify coverage on the host, where llvm-dwarfdump actually runs.
  bash "$here/verify-coverage.sh" "${out_dir_override:-$here/dist/bundle}"
  exit $?
fi

# Native build (this host, or inside the cross-build container).
# NOTE: harness-binary debug info does NOT drive coverage — crucible collects coverage from
# the SVM executing velocity.so, not from the host harness binary — so the host build stays
# lean (no debuginfo). Source-level coverage depends on the program .so's DWARF instead (see
# the SVM staging below / SymbolsPathInBundle).
#  - allow-multiple-definition: velocity's host entrypoint symbol collides under
#    rust-lld on Linux; the GNU-ld/lld flag is REJECTED by macOS ld64, which
#    already tolerates the duplicate symbol — so only add it on Linux.
#  - CARGO_INCREMENTAL=0: switching --features in one target dir confuses incremental
if [ "$host_os" = "Linux" ]; then
  export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,--allow-multiple-definition"
fi
export CARGO_INCREMENTAL=0

# If we reach here on a non-linux/amd64 host, it's a forced --native build; the
# binaries won't run on AR fuzzing workers. Warn but don't fail (still validates the pipeline).
if [ "$skip_build" -eq 0 ] && [ "$is_linux_amd64" -eq 0 ] && [ "$in_container" != "1" ]; then
  echo "WARNING: --native build on ${host_os}/${host_arch}: binaries are NOT deployable to" >&2
  echo "         AR fuzzing workers (linux/amd64). Use the container cross-build for a real bundle." >&2
fi

echo ">> staging bundle at $out_dir (commit ${commit:0:12})"
rm -rf "$out_dir"
mkdir -p "$out_dir/bin"

ZIG_TARGET="x86_64-unknown-linux-gnu"   # matches the manifest's Architecture: amd64
need_so=0
need_srcs=0

# `lineage_suffix` is the optional 4th field (corpus generation). It must be read
# into its own variable: with a 3-variable `read`, bash puts the whole remainder of
# the line into `feature`, so `--features "$feature"` becomes two words and cargo
# rejects the suffix as an unknown feature. Only the manifest generator uses it.
while read -r tier crate feature lineage_suffix; do
  case "$tier" in svm|lf) ;; *) continue ;; esac
  : "${lineage_suffix:=}"
  if [ "$no_svm" -eq 1 ] && [ "$tier" = "svm" ]; then
    echo ">> skip (--no-svm): $crate $feature"
    continue
  fi
  if [ "$svm_only" -eq 1 ] && [ "$tier" = "lf" ]; then
    echo ">> skip (--svm-only): $crate $feature"
    continue
  fi
  [ "$tier" = "svm" ] && { need_so=1; need_srcs=1; }

  crate_dir="$repo_root/fuzz/$crate"

  if [ "$zigbuild" -eq 1 ]; then
    bin_src="$crate_dir/target/$ZIG_TARGET/release/invariant_test"
  else
    bin_src="$crate_dir/target/release/invariant_test"
  fi

  if [ "$skip_build" -eq 0 ]; then
    echo ">> build $crate --features $feature"
    (
      cd "$crate_dir"
      pkg=$(grep -m1 '^name = ' Cargo.toml | sed -E 's/name = "([^"]+)"/\1/')
      # Crucible's proc-macro emits a per-feature `main`/`__CRUCIBLE_ALLOC`;
      # rebuilding the bin in place after a --features switch can leave stale
      # codegen units (spurious E0428). Drop just this crate's artifacts (deps
      # stay cached) so each feature builds from a clean bin — matches how
      # .github/workflows/fuzz.yml compiles every feature.
      # Retry transient cargo build-script failures. The classic one here is
      # "crate `semver` required to be available in rlib format" while compiling a
      # dependency's build script (e.g. curve25519-dalek) — a known-flaky cargo
      # hiccup that succeeds on a re-run. Retry up to 3x before giving up.
      for attempt in 1 2 3; do
        cargo clean -p "$pkg" || true
        if [ "$zigbuild" -eq 1 ]; then
          build_cmd=(cargo zigbuild --release --features "$feature" --target "$ZIG_TARGET")
        else
          build_cmd=(cargo build --release --features "$feature")
        fi
        if "${build_cmd[@]}"; then
          break
        fi
        if [ "$attempt" -eq 3 ]; then
          echo "   FAILED after 3 attempts: $pkg --features $feature" >&2
          exit 1
        fi
        echo "   transient build failure (attempt $attempt) — retrying $pkg --features $feature" >&2
      done
    )
  fi
  [ -f "$bin_src" ] || { echo "missing built binary: $bin_src" >&2; exit 1; }

  dest_dir="$out_dir/bin/$crate"
  mkdir -p "$dest_dir"
  cp "$bin_src" "$dest_dir/$feature"
  echo "   staged bin/$crate/$feature"
done < <(grep -vE '^\s*(#|$)' "$targets_file")

# SVM tier needs the .so at the bundle-root target/deploy/. Two files: velocity.so
# (stripped, loaded by the harness for SVM execution via ../../target/deploy/velocity.so)
# and velocity.debug.so (unstripped DWARF, the coverage SymbolsPathInBundle). Both come
# from the same build so PC addresses line up.
if [ "$need_so" -eq 1 ]; then
  so_src="$repo_root/target/deploy/velocity.so"
  dbg_src="$repo_root/target/deploy/velocity.debug.so"
  if [ ! -f "$so_src" ]; then
    echo "" >&2
    echo "ERROR: SVM targets require $so_src — the container build produces it; if you" >&2
    echo "       pre-built by hand use the fuzz/build-bundle.sh .so step, or --no-svm." >&2
    exit 1
  fi
  mkdir -p "$out_dir/target/deploy"
  cp "$so_src" "$out_dir/target/deploy/velocity.so"
  echo "   staged target/deploy/velocity.so"
  if [ -f "$dbg_src" ]; then
    # Stage under symbols/ (NOT target/) — crucible splits FUZZ_SYMBOLS at "/target/" to
    # infer a source root; a target/ path corrupts DWARF resolution → 0 source files → FCOV fails.
    mkdir -p "$out_dir/symbols"
    cp "$dbg_src" "$out_dir/symbols/velocity.debug.so"
    echo "   staged symbols/velocity.debug.so (DWARF coverage symbols)"
  else
    echo "   WARNING: velocity.debug.so missing — coverage will be PC-keyed (no source-level FCOV)" >&2
  fi
fi

# Stage the Rust source tree for source-level LCOV. The container-built .so's DWARF keys
# lines as `/src/programs/velocity/src/...`; the driver strips SourcesOriginalPath ("/src")
# then looks the remainder up under srcs/ — so srcs/ mirrors the repo root
# (srcs/programs/velocity/src/...).
if [ "$need_srcs" -eq 1 ]; then
  src_stage="$out_dir/srcs"
  n_src=0
  while IFS= read -r rel; do
    mkdir -p "$src_stage/$(dirname "$rel")"
    cp "$repo_root/$rel" "$src_stage/$rel"
    n_src=$((n_src + 1))
  done < <(cd "$repo_root" && find programs -name '*.rs' -not -path '*/target/*' 2>/dev/null)
  echo "   staged $n_src source files under srcs/ (for SVM coverage)"
fi

gen_manifest "$commit" "$no_svm" "$svm_only" > "$out_dir/manifest.fc.json"
echo "   wrote manifest.fc.json ($(grep -c '"Name"' "$out_dir/manifest.fc.json") entries incl. confs)"

# --- Coverage sanity gate ------------------------------------------------------
# Runs on the HOST, never inside the build container. The container image ships
# no llvm-dwarfdump, it runs as a non-root user so it cannot install one, and a
# host llvm bind-mounted in does not execute (Ubuntu-noble binaries against a
# Debian-bookworm runtime) -- it returns EMPTY output, which made this gate
# report "no compile-unit paths readable" for a .so that is in fact perfect
# (60 compile units, DW_AT_comp_dir "/src"). The outer invocation re-runs the
# script with --verify-coverage once the container exits.
if [ "${FUZZ_BUNDLE_IN_CONTAINER:-0}" = "1" ]; then
  echo "   coverage gate deferred to the host (no usable dwarfdump in-container)"
else
# The #1 way a bundle ships with EMPTY source-level coverage: the manifest's
# SourcesOriginalPath does not match the DWARF the .so carries. The crucible LCOV
# keys each line on an absolute path = <DW_AT_comp_dir> + relative-file, and the
# driver strips SourcesOriginalPath from it. If SourcesOriginalPath and comp_dir
# don't share a root (e.g. a host-built .so with comp_dir=/Users/... while the
# manifest says /src), the cover task fails "does not match any source file" and
# coverage renders 0 lines — silently. Catch it here, at build time.
sym="$out_dir/symbols/velocity.debug.so"
if [ -f "$sym" ]; then
  dwdump="$(command -v llvm-dwarfdump || command -v dwarfdump || true)"
  # Debian's `llvm` package installs a VERSIONED llvm-dwarfdump-<N> and NO
  # unsuffixed alias, so `command -v llvm-dwarfdump` finds nothing on a runner
  # that did install llvm -- and this gate then silently skips itself, which is
  # how two other harnesses in this fleet shipped bundles that render empty.
  if [ -z "$dwdump" ]; then
    for c in /usr/lib/llvm-*/bin/llvm-dwarfdump /usr/bin/llvm-dwarfdump-*; do
      [ -x "$c" ] && dwdump="$c" && break
    done
  fi
  if [ -n "$dwdump" ]; then
    # `|| true` on every one of these: with `set -o pipefail` a grep that matches
    # nothing fails the whole pipeline, the assignment fails, and `set -e` kills the
    # script SILENTLY -- no message, exit 1, right after "wrote manifest.fc.json".
    # That is exactly what happened here: velocity's DWARF carries no comp_dir, the
    # comp_dir grep matched nothing, and the gate aborted the build without saying so.
    sop="$(grep -oE '"SourcesOriginalPath": *"[^"]*"' "$out_dir/manifest.fc.json" | head -1 | sed -E 's/.*"SourcesOriginalPath": *"([^"]*)".*/\1/' || true)"
    # The program crate's own comp_dir (exclude dependency/toolchain units).
    compdir="$("$dwdump" --debug-info "$sym" 2>/dev/null | grep -oE 'DW_AT_comp_dir[^"]*"[^"]+"' | sed -E 's/.*"([^"]+)".*/\1/' | grep -viE '\.cargo|/rustc|/toolchains|platform-tools|bpf-tools' | head -1 || true)"
    if [ -n "$sop" ] && [ -n "$compdir" ]; then
      case "$compdir" in "$sop"*) ok=1 ;; *) case "$sop" in "$compdir"*) ok=1 ;; *) ok=0 ;; esac ;; esac
      if [ "${ok:-0}" -ne 1 ]; then
        echo "" >&2
        echo "ERROR: coverage would render EMPTY. SourcesOriginalPath ('$sop') and the .so's DWARF" >&2
        echo "       comp_dir ('$compdir') do not share a root. Build the coverage .so at a fixed" >&2
        echo "       root (container /src or --remap-path-prefix) and set SourcesOriginalPath to match." >&2
        echo "       See fuzz/README.md (coverage build + SourcesOriginalPath)." >&2
        exit 1
      fi
      echo "   coverage sanity OK (SourcesOriginalPath '$sop' consistent with comp_dir '$compdir')"
    else
      # comp_dir is FREQUENTLY ABSENT from SBF DWARF -- verified on real artifacts,
      # where the compile-unit paths are a mixture of repo-relative and bare
      # `src/lib.rs` (dependencies) with no comp_dir at all. Skipping here is what
      # let two sibling harnesses upload bundles the server then rejected with
      # "SourcesOriginalPath ... does not match any source file", rendering
      # lines_found: 0 while every CI step stayed green. So fall back to the
      # authoritative check: does the prefix actually match a compile-unit path?
      if [ -z "$sop" ]; then
        echo "ERROR: no SourcesOriginalPath in the manifest -- coverage cannot render." >&2
        exit 1
      fi
      units="$("$dwdump" --debug-info "$sym" 2>/dev/null \
        | grep -oE 'DW_AT_name[[:space:]]*\("[^"]*\.rs[^"]*"\)' \
        | sed -E 's/.*\("([^"]*)"\)/\1/; s#/@/.*##' | sort -u || true)"
      if [ -z "$units" ]; then
        echo "ERROR: no compile-unit paths readable from $sym; cannot prove that" >&2
        echo "       SourcesOriginalPath ('$sop') matches the coverage profile." >&2
        echo "       Refusing to ship a bundle whose coverage may render empty." >&2
        exit 1
      fi
      if printf '%s\n' "$units" | grep -q "^${sop%/}/"; then
        echo "   coverage sanity OK (no comp_dir; '$sop' matches $(printf '%s\n' "$units" | grep -c "^${sop%/}/") compile-unit paths)"
      else
        echo "ERROR: coverage would render EMPTY. SourcesOriginalPath ('$sop') prefixes NONE" >&2
        echo "       of the $(printf '%s\n' "$units" | wc -l | tr -d ' ') compile-unit paths in the DWARF, e.g.:" >&2
        printf '%s\n' "$units" | head -3 | sed 's/^/         /' >&2
        exit 1
      fi
    fi
  else
    echo "ERROR: no llvm-dwarfdump/dwarfdump available, so the coverage prefix cannot be" >&2
    echo "       verified. That check is the only thing standing between a green build and" >&2
    echo "       a campaign that renders no coverage at all -- install llvm and re-run." >&2
    exit 1
  fi
fi
fi
# -------------------------------------------------------------------------------

if [ "$do_zip" -eq 1 ] && ! command -v zip >/dev/null 2>&1; then
  echo "   note: zip not installed; skipping archive (fuzz-up accepts the bundle dir directly)" >&2
  do_zip=0
fi
if [ "$do_zip" -eq 1 ]; then
  dist="$here/dist"
  zip_path="$dist/velocity-fuzz-bundle.zip"
  rm -f "$zip_path"
  ( cd "$out_dir" && zip -qr "$zip_path" . )
  echo ">> zipped $zip_path"
fi

echo ">> done."
