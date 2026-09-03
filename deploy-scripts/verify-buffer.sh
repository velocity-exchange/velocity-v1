#!/usr/bin/env bash
#
# verify-buffer.sh — independently verify a GitHub Actions deploy.
#
# A multisig signer should NOT trust the buffer hash printed by CI. This script
# reproduces it from source:
#
#   1. Builds <program> locally with `solana-verify build` (verifiable, in the
#      same docker image CI uses) and hashes the resulting .so.
#   2. Pulls the on-chain buffer address the deploy run logged, either from a
#      run URL you pass or from the newest deploy run it finds for <program>.
#   3. Hashes that on-chain buffer and confirms it matches your local build.
#
# If the hashes match, the buffer the multisig is about to upgrade to is exactly
# what `<program>` compiles to from your current checkout.
#
# If the buffer no longer exists (the upgrade was already executed, which
# consumes it), the deployed program's hash is compared instead.
#
# Requires: solana-verify, the solana CLI, and gh (authenticated) unless
# --buffer is given, all on PATH.
set -euo pipefail

DEFAULT_IMAGE="solanafoundation/solana-verifiable-build:3.1.14"

usage() {
	cat <<USAGE
usage: $0 <program> [<actions-run-or-job-url>] [options]

Reproduce a deploy buffer's hash from source and compare it to what is on
chain. Run this before approving a multisig upgrade.

With no run URL and no --buffer it finds the buffer itself: the newest
successful run of release-program.yaml (or manual-devnet-deploy.yaml with
--devnet) that deployed <program>.

arguments:
  <program>                 velocity | token_faucet | jit_proxy (cargo library name)
  <actions-run-or-job-url>  https://github.com/<org>/<repo>/actions/runs/<id>[/job/<id>]
                            (optional: the latest deploy run is used instead)

options:
  --buffer <pubkey>         verify this buffer instead of reading a run log
  --devnet                  strip mainnet-beta + enable the audit-gated
                            features (velocity devnet build flavor)
  --rpc <url>               RPC used to read the on-chain buffer
                            (default: solana CLI config)
  --program-id <pubkey>     program id to fall back to when the buffer is
                            already applied (default: Anchor.toml devnet id)
  --image <docker-image>    verifiable-build image
                            (default: $DEFAULT_IMAGE)
  --skip-build              reuse an existing target/deploy/<program>.so
  --verbose                 stream the docker build output inline
                            (default: one progress line, tail on failure)
  --no-color                plain output (also honours NO_COLOR)
  -h, --help                this text

examples:
  $0 velocity --devnet
  $0 velocity --buffer 9aqTgETnp7FBBrjgH4sx5SLYep7LMJ2fTi5uQ7k6FLso
  $0 velocity https://github.com/org/repo/actions/runs/123 --devnet
USAGE
}

program=""
run_url=""
devnet=0
rpc=""
buffer=""
program_id=""
image="$DEFAULT_IMAGE"
skip_build=0
verbose=0
use_color=1

# Output helpers. Progress goes to stderr and the result block to stdout, so
# the verdict can be piped or captured without the build chatter.

setup_colors() {
	if [ "$use_color" -eq 1 ] && [ -t 2 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
		BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GRN=$'\033[32m'
		YLW=$'\033[33m'; CYN=$'\033[36m'; RST=$'\033[0m'
	else
		BOLD=""; DIM=""; RED=""; GRN=""; YLW=""; CYN=""; RST=""
	fi
}
setup_colors

header() { # $1 = title
	printf '\n%s %s\n' "${CYN}▌${RST}" "${BOLD}$1${RST}" >&2
}

kv()     { printf '   %s%-17s%s %s\n' "$DIM" "$1" "$RST" "$2" >&2; }
detail() { printf '      %s\n' "$*" >&2; }
note()   { printf '      %s\n' "${DIM}$*${RST}" >&2; }
warn()   { printf '      %s %s\n' "${YLW}!${RST}" "$*" >&2; }
die()    { printf '\n%s %s\n' "${RED}error:${RST}" "$*" >&2; exit 1; }

step_no=0
steps_total=0
step() { # $1 = title
	step_no=$((step_no + 1))
	printf '\n%s %s\n' "${CYN}[${step_no}/${steps_total}]${RST}" "${BOLD}$1${RST}" >&2
}

fmt_dur() { # $1 = seconds
	if [ "$1" -ge 60 ]; then printf '%dm%02ds' $(($1 / 60)) $(($1 % 60));
	else printf '%ds' "$1"; fi
}

short_pk() { printf '%s…%s' "${1:0:4}" "${1: -4}"; }

# Run a command behind a single live progress line, or stream it inline with
# --verbose. Output is buffered in a scratch file only so a failure can show
# its tail; it is deleted either way. Returns the command's exit code.
run_step() { # $1 = label, rest = argv
	local label="$1"; shift
	local rc=0 start=$SECONDS pid frame=0 out
	local spin='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'

	if [ "$verbose" -eq 1 ]; then
		"$@" 2>&1 | sed "s/^/      ${DIM}│${RST} /" || true
		rc=${PIPESTATUS[0]}
	else
		out="$(mktemp)"
		"$@" >"$out" 2>&1 &
		pid=$!
		if [ -t 2 ]; then
			while kill -0 "$pid" 2>/dev/null; do
				printf '\r      %s %s %s' \
					"${CYN}${spin:$frame:1}${RST}" "$label" \
					"${DIM}$(fmt_dur $((SECONDS - start)))${RST}" >&2
				frame=$(((frame + 1) % 10))
				sleep 0.2
			done
			printf '\r\033[2K' >&2
		else
			detail "${label}…"
		fi
		wait "$pid" || rc=$?
	fi

	local took; took="$(fmt_dur $((SECONDS - start)))"
	if [ "$rc" -eq 0 ]; then
		printf '      %s %s %s\n' "${GRN}✓${RST}" "$label" "${DIM}${took}${RST}" >&2
	else
		printf '      %s %s %s\n' "${RED}✗${RST}" "$label" "${DIM}${took}, exit ${rc}${RST}" >&2
		if [ -n "${out:-}" ]; then
			printf '      %s\n' "${DIM}last 30 lines (--verbose for all of it):${RST}" >&2
			tail -30 "$out" | sed "s/^/      ${DIM}│${RST} /" >&2 || true
		fi
	fi
	[ -z "${out:-}" ] || rm -f "$out"
	return "$rc"
}


# Newest successful deploy run for this program, when no run URL was given.
# Mainnet releases are tag-driven (program-<name>-<version>), so the tag alone
# rules out other programs; devnet dispatches carry no such metadata, so each
# candidate's log is checked for the deploy summary's own "program:" line.
# Sets gh_log and picked_run.
discover_latest_run() {
	# Herestrings, not pipes, for the log checks below: `grep -q` exits on the
	# first match and the resulting SIGPIPE would fail a pipeline under pipefail.
	local wf repo list id branch created jid log n=0
	if [ "$devnet" -eq 1 ] || [ "$program" = "token_faucet" ]; then
		wf="manual-devnet-deploy.yaml"
	else
		wf="release-program.yaml"
	fi
	detail "workflow ${BOLD}${wf}${RST}"

	repo="$(gh repo view --json nameWithOwner -q .nameWithOwner)" ||
		die "could not resolve the GitHub repo (run this inside the checkout, or pass a run URL)"

	list="$(gh run list --workflow "$wf" --limit 20 \
		--json databaseId,headBranch,createdAt,conclusion \
		--jq '.[] | select(.conclusion == "success") | "\(.databaseId)\t\(.headBranch)\t\(.createdAt)"')" ||
		die "gh run list failed for $wf"
	[ -n "$list" ] || die "no successful $wf runs found"
	detail "scanning recent runs…"

	while IFS="$(printf '\t')" read -r id branch created; do
		[ -n "$id" ] || continue
		# A program-* tag names its program; skip the ones for other programs.
		case "$branch" in
			program-"$program"-*) ;;
			program-*) continue ;;
		esac
		n=$((n + 1))
		[ "$n" -le 5 ] || break
		jid="$(gh api "repos/$repo/actions/runs/$id/jobs" --jq '.jobs[0].id' 2>/dev/null || true)"
		[ -n "$jid" ] || continue
		log="$(gh run view --job "$jid" --log 2>/dev/null || true)"
		if ! grep -qiE "program:[[:space:]]+${program}[[:space:]]*$" <<<"$log"; then
			note "run $id ($created) deployed another program, skipping"
			continue
		fi
		grep -qiE 'program buffer:[[:space:]]+[1-9A-HJ-NP-Za-km-z]{32,44}' <<<"$log" || continue
		gh_log="$log"
		picked_run="https://github.com/$repo/actions/runs/$id"
		detail "run      ${BOLD}${id}${RST} ${DIM}(${branch}, ${created})${RST}"
		detail "${DIM}${picked_run}${RST}"
		return 0
	done <<LIST
$list
LIST

	die "no recent successful $wf run deployed $program (pass a run URL or --buffer)"
}

need_value() { [ $# -ge 2 ] || die "$1 requires a value (got none — is the variable you passed set?)"; }

while [ $# -gt 0 ]; do
	case "$1" in
		--devnet) devnet=1; shift ;;
		--rpc) need_value "$@"; rpc="$2"; shift 2 ;;
		--buffer) need_value "$@"; buffer="$2"; shift 2 ;;
		--program-id) need_value "$@"; program_id="$2"; shift 2 ;;
		--image) need_value "$@"; image="$2"; shift 2 ;;
		--skip-build) skip_build=1; shift ;;
		--verbose) verbose=1; shift ;;
		--no-color) use_color=0; setup_colors; shift ;;
		-h|--help) usage; exit 0 ;;
		-*) usage >&2; die "unknown option: $1" ;;
		*)
			if [ -z "$program" ]; then program="$1"
			elif [ -z "$run_url" ]; then run_url="$1"
			else die "unexpected argument: $1"; fi
			shift ;;
	esac
done

[ -n "$program" ] || { usage >&2; die "missing <program> (velocity | token_faucet | jit_proxy)"; }

command -v solana-verify >/dev/null || die "solana-verify not found on PATH"

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
cd "$repo_root"

flavor="mainnet"
flavor_desc="mainnet, default features"
if [ "$program" = "velocity" ] && [ "$devnet" -eq 1 ]; then
	flavor="devnet"
	flavor_desc="devnet, no mainnet-beta, audit-gated features on"
fi

# resolve-from-log + hash local + hash on-chain, plus the build unless skipped.
steps_total=3
[ -n "$buffer" ] || steps_total=$((steps_total + 1))

header "verify-buffer $program"
if [ -n "$buffer" ]; then
	kv "buffer" "$buffer"
elif [ -n "$run_url" ]; then
	kv "buffer" "${DIM}from the run you passed${RST}"
else
	kv "buffer" "${DIM}latest deploy run for $program${RST}"
fi
kv "build flavor" "$flavor_desc"
kv "image" "$image"
kv "rpc" "${rpc:-${DIM}solana CLI config${RST}}"

# 1. resolve the on-chain buffer address.
logged_hash=""
picked_run=""
if [ -z "$buffer" ]; then
	step "resolve buffer address from GitHub Actions"
	command -v gh >/dev/null || die "gh not found on PATH (needed to read the run log)"
	if [ -n "$run_url" ]; then
		# Accept .../runs/<runId>[/job/<jobId>]; prefer the job id when present.
		job_id="$(printf '%s' "$run_url" | sed -nE 's#.*/job/([0-9]+).*#\1#p')"
		run_id="$(printf '%s' "$run_url" | sed -nE 's#.*/runs/([0-9]+).*#\1#p')"
		[ -n "$job_id" ] || [ -n "$run_id" ] || die "could not parse a run/job id from: $run_url"
		detail "fetching the log for $([ -n "$job_id" ] && echo "job $job_id" || echo "run $run_id")…"
		if [ -n "$job_id" ]; then
			gh_log="$(gh run view --job "$job_id" --log)" || die "gh run view failed"
		else
			gh_log="$(gh run view "$run_id" --log)" || die "gh run view failed"
		fi
	else
		discover_latest_run
	fi

	# buffer-deploy logs the program (BPF) buffer address as `program buffer: <addr>`.
	# Match case-insensitively with flexible spacing so it works across log formats
	# ("Program buffer:" from the old collect step and "program buffer:" from the
	# newer consolidated summary). The trailing ":" right after "buffer" keeps this
	# from matching the "program buffer hash:" line. Take the last match.
	buffer="$(grep -oiE 'program buffer:[[:space:]]+[1-9A-HJ-NP-Za-km-z]{32,44}' <<<"$gh_log" | tail -1 | awk '{print $NF}')"
	[ -n "$buffer" ] || die "could not find a 'program buffer:' address in the run log"
	# And the hash CI logged, for an extra cross-check (sha256 hex). Matches both
	# the old "Buffer hash:" and the new "program buffer hash:" lines.
	logged_hash="$(grep -oiE 'buffer hash:[[:space:]]+[0-9a-f]{64}' <<<"$gh_log" | tail -1 | awk '{print $NF}' || true)"
	detail "buffer   ${BOLD}${buffer}${RST}"
	if [ -n "$logged_hash" ]; then detail "ci hash  ${DIM}${logged_hash}${RST}"; fi
fi

# 2. verifiable build + local hash.
so_path="$repo_root/target/deploy/${program}.so"
if [ "$skip_build" -eq 0 ]; then
	step "verifiable build, $flavor flavor"
	note "docker, this takes a few minutes"
	# velocity's devnet build drops mainnet-beta (production gates off, devnet-only
	# ixs compiled in) and enables the audit-gated features, mirroring
	# .github/actions/build-program — the flag sets must stay identical or the
	# hashes diverge.
	if [ "$program" = "velocity" ] && [ "$devnet" -eq 1 ]; then
		run_step "solana-verify build" \
			solana-verify build --library-name "$program" -b "$image" \
			-- --no-default-features --features no-entrypoint,isolated-position,vlp-hedge ||
			die "verifiable build failed"
	else
		run_step "solana-verify build" \
			solana-verify build --library-name "$program" -b "$image" ||
			die "verifiable build failed"
	fi
else
	step "local artifact (--skip-build)"
	note "reusing ${so_path#"$repo_root/"}"
fi
[ -f "$so_path" ] || die "built artifact not found: $so_path"

step "hash the local artifact"
detail "${so_path#"$repo_root/"} ${DIM}($(du -h "$so_path" | awk '{print $1}'))${RST}"
local_hash="$(solana-verify get-executable-hash "$so_path" 2>/dev/null | grep -oE '[0-9a-f]{64}' | tail -1 || true)"
[ -n "$local_hash" ] || die "could not hash the local build at $so_path"
detail "${GRN}✓${RST} $local_hash"

# 3. on-chain hash, then compare.
step "hash the on-chain buffer"

# solana-verify reads the CLI config file; make sure one exists.
if [ -n "$rpc" ]; then solana config set --url "$rpc" >/dev/null 2>&1 || true; fi

# Hash an on-chain account; prints a 64-hex hash or nothing on failure.
on_chain_hash() {
	# $1 = subcommand (get-buffer-hash | get-program-hash), $2 = pubkey
	if [ -n "$rpc" ]; then
		solana-verify "$1" --url "$rpc" "$2" 2>/dev/null || true
	else
		solana-verify "$1" "$2" 2>/dev/null || true
	fi | grep -oE '[0-9a-f]{64}' | tail -1 || true
}

target="buffer $buffer"
target_short="buffer $(short_pk "$buffer")"
detail "buffer ${BOLD}${buffer}${RST}"
onchain_hash="$(on_chain_hash get-buffer-hash "$buffer")"

if [ -z "$onchain_hash" ]; then
	# A buffer disappears once the upgrade is executed (the BPF loader consumes
	# it). In that case verify the DEPLOYED PROGRAM instead — the more useful
	# check post-execution: does what's live match this source?
	warn "buffer not found on chain — it was likely already applied (the upgrade consumes it)"
	note "falling back to the deployed program hash"
	if [ -z "$program_id" ]; then
		# Resolve <program>'s id from Anchor.toml [programs.devnet].
		program_id="$(awk -F'"' -v p="$program" '
			/^\[programs\.devnet\]/ { s = 1; next }
			/^\[/                   { s = 0 }
			s && $1 ~ "^"p"[[:space:]]*=" { print $2; exit }
		' "$repo_root/Anchor.toml")"
	fi
	[ -n "$program_id" ] ||
		die "buffer is gone and the program id is unknown — pass --program-id <pubkey> to verify the deployed program"
	target="deployed program $program_id"
	target_short="deployed program $(short_pk "$program_id")"
	detail "program ${BOLD}${program_id}${RST}"
	onchain_hash="$(on_chain_hash get-program-hash "$program_id")"
	[ -n "$onchain_hash" ] ||
		die "could not hash the buffer ($buffer) or the deployed program ($program_id) at the given RPC"
fi
detail "${GRN}✓${RST} $onchain_hash"

if [ "$local_hash" = "$onchain_hash" ]; then hc="$GRN"; else hc="$RED"; fi

header "result"
printf '   %s%-17s%s %s\n' "$DIM" "program" "$RST" "$program"
printf '   %s%-17s%s %s\n' "$DIM" "build flavor" "$RST" "$flavor_desc"
printf '   %s%-17s%s %s\n' "$DIM" "compared against" "$RST" "$target"
if [ -n "$picked_run" ]; then
	printf '   %s%-17s%s %s\n' "$DIM" "deploy run" "$RST" "$picked_run"
fi
printf '   %s%-17s%s %s%s%s\n' "$DIM" "local build" "$RST" "$hc" "$local_hash" "$RST"
printf '   %s%-17s%s %s%s%s\n' "$DIM" "on chain" "$RST" "$hc" "$onchain_hash" "$RST"
if [ -n "$logged_hash" ]; then
	if [ "$logged_hash" = "$onchain_hash" ]; then
		printf '   %s%-17s%s %s %s\n' "$DIM" "logged by ci" "$RST" "$logged_hash" "${DIM}(agrees)${RST}"
	else
		printf '   %s%-17s%s %s%s%s %s\n' "$DIM" "logged by ci" "$RST" \
			"$YLW" "$logged_hash" "$RST" "${YLW}(disagrees with chain, buffer may have been re-staged since)${RST}"
	fi
fi
echo

if [ "$local_hash" = "$onchain_hash" ]; then
	printf '   %s  %s\n\n' "${GRN}${BOLD}✓ MATCH${RST}" "$target_short is exactly this source's verifiable build."
	exit 0
else
	printf '   %s  %s\n' "${RED}${BOLD}✗ MISMATCH${RST}" "$target_short does not match this source build."
	printf '   %s\n' "${RED}do NOT approve; investigate first.${RST}"
	printf '   %s\n\n' "${DIM}right commit checked out? right flavor (--devnet)? same --image as CI?${RST}"
	exit 1
fi
