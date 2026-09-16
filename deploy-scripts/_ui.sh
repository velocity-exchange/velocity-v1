# Shared terminal output for the deploy-scripts CLIs (verify-buffer.sh,
# release.sh). Source it; do not execute it.
#
# Progress goes to stderr and result blocks to stdout, so a verdict can be
# piped or captured without the chatter. Colors follow the sourcing script's
# `use_color` (default on) plus NO_COLOR / TERM=dumb; `run_step` streams the
# command when `verbose=1`.
#
#   header "title"          ▌ title
#   kv key value            aligned key/value line under a header
#   detail / note / warn    indented line, dim line, "! warning"
#   die "msg"               error to stderr, exit 1
#   steps_total=N; step "t" [n/N] t
#   run_step "label" cmd…   spinner (TTY) or plain label, ✓/✗ with duration
#   fmt_dur secs, short_pk pubkey

: "${use_color:=1}"
: "${verbose:=0}"

setup_colors() {
	if [ "$use_color" -eq 1 ] && [ -t 2 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
		BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GRN=$'\033[32m'
		YLW=$'\033[33m'; CYN=$'\033[36m'; RST=$'\033[0m'
	else
		BOLD=""; DIM=""; RED=""; GRN=""; YLW=""; CYN=""; RST=""
	fi
}
setup_colors

header() { # $1 = title, $2 = optional dim suffix
	printf '\n%s %s%s\n' "${CYN}▌${RST}" "${BOLD}$1${RST}" "${2:+ ${DIM}$2${RST}}" >&2
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
# verbose=1. Output is buffered in a scratch file only so a failure can show
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
