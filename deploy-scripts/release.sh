#!/usr/bin/env bash
#
# release.sh: the velocity-v1 release CLI. Read-only by default.
#
# One verb per release step, in release order. Every verb prints what it
# found and what --execute would do; with --execute it fires exactly one thing
# (a git push, a gh dispatch, an infra `yarn deploy`) and hands you the URL.
# Nothing here waits on CI, verifies a buffer, approves Squads or merges a PR:
# GitHub, verify-buffer.sh, Squads and ArgoCD already do those.
#
#   status                        where every artifact stands + the next command
#   bump    [prog] [X.Y.Z]        release/<prog>-<ver> branch: Cargo.toml, lockfiles,
#                                 IDLs, commit (your key), push, PR link
#   devnet  [prog] [--branch ref] gh workflow run manual-devnet-deploy.yaml
#   npm     [pkg...]              push npm-<pkg>-v<version> for untagged packages
#   docker  [app...]              push docker-<app>-v<next patch> for changed images
#   infra   <stage...>            infra-v3 `yarn deploy a,b,c <stage>` for stale pins
#   mainnet [prog]                push program-<prog>-v<Cargo version>
#
# Truth is git tags, Cargo.toml, packages/*/package.json, docker-info.json and
# the gitops manifests; there is no state file. Re-running after a failure is
# safe: existing tags abort, pins already current are a no-op.
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
cd "$repo_root"

# shellcheck source=_ui.sh
source "$script_dir/_ui.sh"

usage() {
	cat <<USAGE
usage: $0 [command] [args] [options]

commands (in release order; no command = status)
  status                      programs, npm packages, docker images, gitops
                              pins, and the one command to run next
  bump    [prog] [X.Y.Z]      branch off origin/master, bump Cargo.toml +
                              lockfiles + IDLs, commit (your key), push, PR link
                              (default: velocity, next minor)
  devnet  [prog]              dispatch manual-devnet-deploy.yaml   [--branch <ref>]
  npm     [pkg...]            tag every untagged packages/<pkg> version, push
  docker  [app...]            tag docker-<app>-v<next patch> for images with
                              changes since their tag (or the apps named), push
  infra   <stage...>          infra-v3 \`yarn deploy a,b,c <stage>\` for every
                              image whose gitops pin is behind, one PR
  mainnet [prog]              push program-<prog>-v<Cargo version> at origin/master

options
  --execute        do it (default: dry run, read-only)
  --infra <path>   infrastructure-v3 checkout (or VELOCITY_INFRA_DIR)
  --branch <ref>   devnet: build this ref instead of master
  --no-fetch       skip the initial \`git fetch origin master --tags\`
  --no-color       plain output (also honours NO_COLOR)
  -h, --help       this text

programs: velocity (token_faucet: devnet only)
USAGE
}

execute=0
infra_dir="${VELOCITY_INFRA_DIR:-}"
do_fetch=1
branch_ref="master"
cmd=""
args=()

need_value() { [ $# -ge 2 ] || die "$1 requires a value"; }

while [ $# -gt 0 ]; do
	case "$1" in
		--execute) execute=1; shift ;;
		--infra) need_value "$@"; infra_dir="$2"; shift 2 ;;
		--branch) need_value "$@"; branch_ref="$2"; shift 2 ;;
		--no-fetch) do_fetch=0; shift ;;
		--no-color) use_color=0; setup_colors; shift ;;
		-h|--help) usage; exit 0 ;;
		-*) usage >&2; die "unknown option: $1" ;;
		*) if [ -z "$cmd" ]; then cmd="$1"; else args+=("$1"); fi; shift ;;
	esac
done
cmd="${cmd:-status}"

for tool in git gh jq; do
	command -v "$tool" >/dev/null || die "$tool not found on PATH"
done

MASTER="origin/master"
DEVNET_WF="manual-devnet-deploy.yaml"
MAINNET_WF="release-program.yaml"
NPM_WF="npm-publish.yml"
DOCKER_WF="velocity-publish.yml"
# npm-publish.yml's `if:` skips these packages' tags (not public yet).
NPM_CI_SKIPPED=" vaults-sdk cli-admin "
PROGRAMS="velocity token_faucet"
MAINNET_PROGRAMS="velocity"

program_path()  { case "$1" in velocity) echo programs/velocity ;; token_faucet) echo programs/token_faucet ;; esac; }
program_crate() { case "$1" in velocity) echo velocity ;; token_faucet) echo token_faucet ;; esac; }
program_idl()   { case "$1" in velocity) echo program:idl ;; *) echo "" ;; esac; }
program_idl_files() {
	case "$1" in
		velocity) echo "packages/sdk/src/idl/velocity.json packages/sdk/src/idl/velocity.ts" ;;
	esac
}
check_program() { # $1 = name, $2 = allowed list
	case " $2 " in *" $1 "*) ;; *) die "unknown program \"$1\" (known: $(echo "$2" | tr ' ' ','))" ;; esac
}

gh_repo_cache=""
gh_repo() {
	[ -n "$gh_repo_cache" ] || gh_repo_cache="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
	echo "$gh_repo_cache"
}
short()          { printf '%s' "${1:0:7}"; }
master_sha()     { git rev-parse "$MASTER"; }
tag_version()    { printf '%s' "${1##*-v}"; }
tag_sha()        { git rev-list -n1 "$1" 2>/dev/null || true; }
tag_date()       { git log -1 --format=%cs "$1" 2>/dev/null || true; }
latest_tag()     { git tag --list "${1}v*" | grep -E 'v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1; }
tag_exists()     { [ -n "$(git tag --list "$1")" ]; }
file_at()        { git show "$MASTER:$1" 2>/dev/null || true; }
cargo_version()  { file_at "$(program_path "$1")/Cargo.toml" | sed -nE 's/^version *= *"([^"]+)".*/\1/p' | head -1; }
commits_since()  { # $1 = sha, rest = paths
	local sha="$1"; shift
	[ -n "$sha" ] || return 0
	git log --format='%h %s' "$sha..$MASTER" -- "$@"
}
count_lines()    { if [ -z "$1" ]; then echo 0; else printf '%s\n' "$1" | wc -l | tr -d ' '; fi; }
semver_gt()      { [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]; }
next_minor()     { local a b; IFS=. read -r a b _ <<<"$1"; echo "$a.$((b + 1)).0"; }
next_patch()     { local a b c; IFS=. read -r a b c <<<"$1"; echo "$a.$b.$((c + 1))"; }
is_semver()      { [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; }
actions_url()    { echo "https://github.com/$(gh_repo)/actions/workflows/$1"; }
mark()           { case "$1" in success) printf '%s' "${GRN}✓${RST}" ;; in_progress|queued|"") printf '%s' "${YLW}…${RST}" ;; *) printf '%s' "${RED}✗${RST}" ;; esac; }

# `gh run list` as tab lines: headBranch, headSha, status, conclusion, createdAt, url
runs() { # $1 = workflow, $2 = limit
	gh run list --workflow "$1" --limit "${2:-20}" \
		--json headBranch,headSha,status,conclusion,createdAt,url \
		--jq '.[] | [.headBranch, .headSha, .status, (.conclusion // ""), .createdAt, .url] | @tsv' 2>/dev/null || true
}
run_for_branch() { runs "$1" 30 | awk -F'\t' -v b="$2" '$1 == b && !f { print; f = 1 }'; }
latest_run()     { runs "$1" 1; }

# The one mutation gate. Prints the command either way; runs it only with
# --execute. Commands inherit the terminal (gpg pinentry, yarn output).
mutate() { # [-C dir] cmd...
	local dir="$repo_root"
	if [ "$1" = "-C" ]; then dir="$2"; shift 2; fi
	if [ "$execute" -eq 1 ]; then
		printf '      %s %s\n' "${DIM}\$${RST}" "$*" >&2
		(cd "$dir" && "$@")
	else
		printf '      %s %s\n' "${YLW}would run${RST}" "$*" >&2
	fi
}

# Warm the gpg-agent cache before a signed commit so pinentry asks on its own
# line, not after screens of other output where a missed prompt times out.
unlock_signing_key() { # $1 = repo dir
	[ "$(git -C "$1" config --get commit.gpgsign 2>/dev/null)" = "true" ] || return 0
	[ "$execute" -eq 1 ] || return 0
	detail "${DIM}\$${RST} gpg --clearsign  ${DIM}(unlock your signing key; pinentry asks here)${RST}"
	echo | gpg --clearsign >/dev/null
}

# Push tags one per push: GitHub emits no push events at all when a single
# push carries more than three tags, so a batched push silently skips every
# tag-triggered workflow.
push_tags() { for t in "$@"; do mutate git push origin "$t"; done; }

dry_run_kv() { [ "$execute" -eq 1 ] || kv "dry run" "add --execute to do it"; }

# Velocity changes since the last deployed tag that need something staged
# alongside the upgrade. Diff greps, so they fire only when the pending upgrade
# actually carries the change. Prints warnings; echoes the count.
velocity_checks() { # $1 = from sha
	local from="$1" n=0 src=programs/velocity/src
	[ -n "$from" ] || { echo 0; return; }
	if git diff "$from..$MASTER" -- "$src/state/state.rs" "$src/math/fees.rs" "$src/math/constants.rs" \
		| grep -E '^[+-][^+-]' | grep -qE 'perps_default|determine_perp_fee_tier|PERP_FEE_TIER_MAX_INDEX|VOLUME_THRESHOLDS'; then
		warn "fee schedule code changed since the last tag: run \`velocity-admin fees set-schedule\` BEFORE the upgrade (State keeps the old tiers; see README)"
		n=$((n + 1))
	fi
	if ! git diff --quiet "$from..$MASTER" -- "$src/error.rs"; then
		warn "error enum changed since the last tag: confirm variants were only appended (ABI-stable codes)"
		n=$((n + 1))
	fi
	if git diff "$from..$MASTER" -- packages/sdk/src/idl/velocity.json | grep -E '^[+-][^+-]' | grep -vq '"version"'; then
		warn "IDL changed since the last tag: bots and SDK consumers need the new @velocity-exchange/sdk"
		n=$((n + 1))
	fi
	echo "$n"
}

# docker apps from docker-info.json: "app<TAB>lang<TAB>path"
docker_apps() {
	jq -r 'to_entries[] | select(.key | startswith("_") | not) | [.key, .value.lang, .value.path] | @tsv' docker-info.json
}
docker_watch_paths() { # $1 = lang, $2 = path
	if [ "$1" = ts ]; then echo "$2 packages/sdk docker/ts-app.Dockerfile"
	else echo "$2 rust/velocity-rs packages/sdk/src/idl docker/rust-app.Dockerfile"; fi
}

# publishable packages: "dir<TAB>version<TAB>tagged(1/0)<TAB>skipped(1/0)"
npm_packages() {
	local d json ver tag tagged skipped
	for d in $(git ls-tree --name-only "$MASTER" packages/ | xargs -n1 basename); do
		json="$(file_at "packages/$d/package.json")"
		[ -n "$json" ] || continue
		[ "$(printf '%s' "$json" | jq -r '.private // false')" = "true" ] && continue
		ver="$(printf '%s' "$json" | jq -r .version)"
		tag="npm-$d-v$ver"
		tagged=0; tag_exists "$tag" && tagged=1
		skipped=0; case "$NPM_CI_SKIPPED" in *" $d "*) skipped=1 ;; esac
		printf '%s\t%s\t%s\t%s\n' "$d" "$ver" "$tagged" "$skipped"
	done
}
version_packages_pr() { gh pr list --head changeset-release/master --state open --json number,url --jq '.[0] // empty | "\(.number) \(.url)"' 2>/dev/null || true; }

# gitops stages, read from the branch each ArgoCD actually tracks (non-prod
# follows origin/master, prod follows origin/mainnet-beta), never from the
# working tree, which may be stale or mid-edit: "stage<TAB>env<TAB>ref<TAB>path"
infra_stages() {
	[ -n "$infra_dir" ] && [ -d "$infra_dir/.git" ] || return 0
	local env ref p
	for env in non-prod prod; do
		ref="origin/master"; [ "$env" != prod ] || ref="origin/mainnet-beta"
		git -C "$infra_dir" ls-tree -d --name-only "$ref" "gitops/$env/workloads/" 2>/dev/null \
			| while read -r p; do printf '%s\t%s\t%s\t%s\n' "$(basename "$p")" "$env" "$ref" "$p"; done
	done
}
# distinct X.Y.Z pinned for an image under a stage path at a ref, oldest first
pinned_versions() { # $1 = ref, $2 = stage path, $3 = app
	git -C "$infra_dir" grep -ohE "/$3:v[0-9]+\.[0-9]+\.[0-9]+" "$1" -- "$2" 2>/dev/null | sed 's/.*:v//' | sort -uV || true
}
stage_row() { infra_stages | awk -F'\t' -v s="$1" '$1 == s && !f { print; f = 1 }'; }
fetch_infra() { [ -z "$infra_dir" ] || [ "$do_fetch" -eq 0 ] || git -C "$infra_dir" fetch origin master mainnet-beta --quiet 2>/dev/null || warn "infra fetch failed; using local origin refs"; }

next_cmd=""
next_note=""
set_next() { [ -n "$next_cmd" ] || { next_cmd="$1"; next_note="${2:-}"; }; }

cmd_status() {
	local sha; sha="$(master_sha)"
	header "release status" "$MASTER $(short "$sha")"
	steps_total=4; [ -n "$infra_dir" ] || steps_total=3

	# --- programs
	step "programs"
	local p ver tag tver tsha n mrun mconc drun
	drun="$(latest_run "$DEVNET_WF")"
	for p in $MAINNET_PROGRAMS; do
		ver="$(cargo_version "$p")"
		tag="$(latest_tag "program-$p-")"
		tver=""; tsha=""; [ -z "$tag" ] || { tver="$(tag_version "$tag")"; tsha="$(tag_sha "$tag")"; }
		n="$(count_lines "$(commits_since "$tsha" "$(program_path "$p")")")"
		mconc=""; [ -z "$tag" ] || mconc="$(run_for_branch "$MAINNET_WF" "$tag" | cut -f4)"
		printf '   %s%-13s%s Cargo %-9s tag v%-8s %s   mainnet %s   %s program commit(s) since tag\n' \
			"$BOLD" "$p" "$RST" "$ver" "${tver:-none}" "${DIM}$(short "$tsha") $(tag_date "$tag")${RST}" "$(mark "$mconc")" "$n" >&2
		if [ -n "$tver" ] && [ "$n" -gt 0 ] && ! semver_gt "$ver" "$tver"; then
			warn "$p: Cargo $ver is not ahead of tag v$tver; bump before tagging"
			set_next "bump $p"
		fi
		[ "$p" = velocity ] && velocity_checks "$tsha" >/dev/null
		[ "$n" -eq 0 ] || commits_since "$tsha" "$(program_path "$p")" | head -5 | sed "s/^/      ${DIM}$p${RST}  /" >&2
	done
	if [ -n "$drun" ]; then
		local dsha dstat dconc durl dbehind
		dsha="$(printf '%s' "$drun" | cut -f2)"; dstat="$(printf '%s' "$drun" | cut -f3)"
		dconc="$(printf '%s' "$drun" | cut -f4)"; durl="$(printf '%s' "$drun" | cut -f6)"
		dbehind="$(git rev-list --count "$dsha..$MASTER" 2>/dev/null || echo '?')"
		detail "devnet last deploy run $(mark "${dconc:-$dstat}") @$(short "$dsha") ${DIM}($dbehind commits behind master) $durl${RST}"
		if [ "$dstat" != completed ]; then set_next "" "wait for the devnet run: $durl"
		elif [ "$dsha" != "$sha" ] || [ "$dconc" != success ]; then set_next "devnet" "then sign the devnet Squads proposal; verify first: bun run verify-buffer velocity --devnet"
		fi
	else
		set_next "devnet"
	fi

	# --- npm
	step "npm packages"
	local vp; vp="$(version_packages_pr)"
	[ -z "$vp" ] || { warn "Version Packages PR #${vp%% *} open: ${vp#* }"; set_next "" "merge the Version Packages PR, then \`release npm\`"; }
	local d pv tagged skipped state
	while IFS=$'\t' read -r d pv tagged skipped; do
		[ -n "$d" ] || continue
		if [ "$tagged" = 1 ]; then state="${GRN}tagged${RST}"
		elif [ "$skipped" = 1 ]; then state="${DIM}untagged (CI skips this package)${RST}"
		else state="${YLW}untagged${RST}"; set_next "npm"; fi
		printf '   %s%-13s%s %-9s %s\n' "$BOLD" "$d" "$RST" "$pv" "$state" >&2
	done <<<"$(npm_packages)"

	# --- docker
	step "docker images"
	local app lang path dtag dver dsha dn dconc
	while IFS=$'\t' read -r app lang path; do
		[ -n "$app" ] || continue
		dtag="$(latest_tag "docker-$app-")"; dver=""; dsha=""
		[ -z "$dtag" ] || { dver="$(tag_version "$dtag")"; dsha="$(tag_sha "$dtag")"; }
		# shellcheck disable=SC2046
		dn="$(count_lines "$(commits_since "$dsha" $(docker_watch_paths "$lang" "$path"))")"
		dconc=""; [ -z "$dtag" ] || dconc="$(run_for_branch "$DOCKER_WF" "$dtag" | cut -f4)"
		printf '   %s%-15s%s v%-8s %s  build %s  %s commit(s) since\n' \
			"$BOLD" "$app" "$RST" "${dver:-?}" "${DIM}$(short "$dsha") $(tag_date "$dtag")${RST}" "$(mark "$dconc")" "$dn" >&2
		[ "$dn" -eq 0 ] && [ -n "$dtag" ] || set_next "docker"
	done <<<"$(docker_apps)"

	# --- gitops pins
	if [ -n "$infra_dir" ]; then
		step "gitops pins" ; note "$infra_dir  (non-prod from origin/master, prod from origin/mainnet-beta)"
		local st env sref spath pins latest cell line stale_master=0 stale_prod=0
		while IFS=$'\t' read -r app lang path; do
			[ -n "$app" ] || continue
			latest="$(tag_version "$(latest_tag "docker-$app-")")"
			line=""
			while IFS=$'\t' read -r st env sref spath; do
				[ -n "$st" ] || continue
				pins="$(pinned_versions "$sref" "$spath" "$app" | tr '\n' '/' | sed 's,/$,,')"
				[ -n "$pins" ] || continue
				if [ -n "$latest" ] && semver_gt "$latest" "${pins##*/}"; then
					cell="$st ${YLW}$pins !${RST}"
					if [ "$env" = prod ]; then stale_prod=1; else stale_master=1; fi
				else cell="$st ${GRN}$pins${RST}"; fi
				line="$line   $cell"
			done <<<"$(infra_stages)"
			printf '   %s%-15s%s latest v%-8s%s\n' "$BOLD" "$app" "$RST" "${latest:-?}" "$line" >&2
		done <<<"$(docker_apps)"
		[ "$stale_master" -eq 0 ] || set_next "infra master"
	fi

	# --- mainnet tag for the current Cargo version
	for p in $MAINNET_PROGRAMS; do
		ver="$(cargo_version "$p")"; tag="program-$p-v$ver"
		if ! tag_exists "$tag"; then
			set_next "mainnet $p" "then sign the mainnet Squads proposal; verify first: bun run verify-buffer $p"
		else
			mrun="$(run_for_branch "$MAINNET_WF" "$tag")"
			[ "$(printf '%s' "$mrun" | cut -f3)" = completed ] || set_next "" "wait for the mainnet run for $tag: $(printf '%s' "$mrun" | cut -f6)"
		fi
	done
	if [ -n "$infra_dir" ] && [ "${stale_prod:-0}" -eq 1 ]; then set_next "infra mainnet-beta"; fi
	if [ -n "$infra_dir" ] && ! git -C "$infra_dir" diff --quiet origin/mainnet-beta origin/master -- gitops/prod 2>/dev/null; then
		set_next "" "infra-v3: prod pins on master not yet released; open the master → mainnet-beta PR: (cd $infra_dir && gh pr create --base mainnet-beta --head master)"
	fi

	header "next"
	if [ -n "$next_cmd" ]; then
		printf '   %s\n' "${BOLD}bash deploy-scripts/release.sh $next_cmd${RST}   ${DIM}(dry run; add --execute)${RST}"
		[ -z "$next_note" ] || printf '   %s\n' "${DIM}$next_note${RST}"
	elif [ -n "$next_note" ]; then
		printf '   %s\n' "$next_note"
	else
		printf '   %s\n' "${GRN}nothing to release${RST}"
	fi
}

cmd_bump() {
	local p="${args[0]:-velocity}" target="${args[1]:-}"
	check_program "$p" "$MAINNET_PROGRAMS"
	local ver tag tver
	ver="$(cargo_version "$p")"; tag="$(latest_tag "program-$p-")"; tver=""
	[ -z "$tag" ] || tver="$(tag_version "$tag")"
	if [ -z "$target" ]; then
		target="$ver"; [ -z "$tver" ] || semver_gt "$ver" "$tver" || target="$tver"
		target="$(next_minor "$target")"
	fi
	is_semver "$target" || die "not a version: $target"
	semver_gt "$target" "$ver" || die "$target is not ahead of Cargo $ver"
	[ -z "$tver" ] || semver_gt "$target" "$tver" || die "$target is not ahead of the last tag v$tver"

	local branch="release/$p-$target" crate ppath files
	crate="$(program_crate "$p")"; ppath="$(program_path "$p")"
	files="$ppath/Cargo.toml Cargo.lock $(program_idl_files "$p")"
	[ "$p" = velocity ] && files="$files rust/Cargo.lock rust/velocity-rs/crates/src/velocity_idl.rs"

	header "release bump $p" "$([ "$execute" -eq 1 ] || echo '(dry run)')"
	kv "current" "Cargo $ver, tag ${tag:-none}"
	kv "target" "$target"
	kv "branch" "$branch from $MASTER"
	kv "files" "$(echo "$files" | tr ' ' '\n' | sed '2,$s/^/                     /')"
	steps_total=5; [ "$p" = velocity ] && steps_total=6

	step "checkout a fresh branch"
	if [ "$execute" -eq 1 ] && [ -n "$(git status --porcelain --untracked-files=no)" ]; then
		die "working tree is dirty; commit or stash first"
	fi
	mutate git switch -c "$branch" "$MASTER"

	step "set version = \"$target\" in $ppath/Cargo.toml"
	mutate perl -pi -e "s/^version\\s*=\\s*\"[^\"]+\"/version = \"$target\"/ if !\$done++" "$ppath/Cargo.toml"

	step "resync the program workspace lockfile (offline)"
	# rust/Cargo.lock cannot resolve offline (solana-sdk 3.x tree); the cargo
	# check below refreshes it as a side effect.
	mutate cargo update -p "$crate" --offline

	step "regenerate the IDL (carries the version)"
	# anchor idl build writes -o target/idl/… and does not create the dir.
	mutate mkdir -p target/idl
	mutate bun run "$(program_idl "$p")"

	if [ "$p" = velocity ]; then
		step "regenerate velocity_idl.rs + rust/Cargo.lock (velocity-rs build.rs)"
		mutate cargo check --manifest-path rust/Cargo.toml -p velocity-rs
	fi

	step "commit and push the branch"
	unlock_signing_key "$repo_root"
	# shellcheck disable=SC2086
	mutate git add $files
	mutate git commit -m "bump $p to $target"
	mutate git push -u origin "$branch"

	header "result"
	kv "open the PR" "https://github.com/$(gh_repo)/compare/master...$branch?expand=1"
	kv "then" "merge it; release.sh devnet builds that sha, release.sh mainnet tags it"
	dry_run_kv
}

cmd_devnet() {
	local p="${args[0]:-velocity}"
	check_program "$p" "$PROGRAMS"
	local sha; sha="$(git rev-parse "origin/$branch_ref" 2>/dev/null)" || die "unknown ref origin/$branch_ref"

	header "release devnet $p" "$([ "$execute" -eq 1 ] || echo '(dry run)')"
	kv "workflow" "$DEVNET_WF"
	kv "branch" "$branch_ref @$(short "$sha")"
	kv "version" "Cargo $(git show "origin/$branch_ref:$(program_path "$p")/Cargo.toml" | sed -nE 's/^version *= *"([^"]+)".*/\1/p' | head -1)"
	[ "$p" != velocity ] || velocity_checks "$(tag_sha "$(latest_tag "program-velocity-")")" >/dev/null

	steps_total=1
	step "dispatch the workflow"
	mutate gh workflow run "$DEVNET_WF" --ref master -f "program=$p" -f "branch=$branch_ref"

	header "result"
	kv "run" "$(actions_url "$DEVNET_WF")"
	kv "when green" "bun run verify-buffer $p --devnet"
	kv "then" "approve + execute the proposal in the devnet Squads"
	dry_run_kv
}

cmd_mainnet() {
	local p="${args[0]:-velocity}"
	check_program "$p" "$MAINNET_PROGRAMS"
	local ver tag last tver sha n
	ver="$(cargo_version "$p")"; tag="program-$p-v$ver"; last="$(latest_tag "program-$p-")"
	sha="$(master_sha)"; tver=""; [ -z "$last" ] || tver="$(tag_version "$last")"
	n="$(count_lines "$(commits_since "$(tag_sha "$last")" "$(program_path "$p")")")"

	header "release mainnet $p" "$([ "$execute" -eq 1 ] || echo '(dry run)')"
	kv "workflow" "$MAINNET_WF"
	kv "tag" "$tag → $MASTER @$(short "$sha")"
	kv "last tag" "${last:-none} ${DIM}($n program commit(s) since)${RST}"
	[ "$p" != velocity ] || velocity_checks "$(tag_sha "$last")" >/dev/null

	[ -n "$tver" ] && ! semver_gt "$ver" "$tver" && die "Cargo $ver is not ahead of $last; land \`release.sh bump $p\` first"
	tag_exists "$tag" && die "$tag already exists"

	steps_total=1
	step "tag origin/master and push the tag"
	mutate git tag "$tag" "$sha"
	push_tags "$tag"

	header "result"
	kv "run" "$(actions_url "$MAINNET_WF")"
	kv "when green" "bun run verify-buffer $p"
	kv "then" "approve + execute the proposal in the mainnet Squads"
	dry_run_kv
}

cmd_npm() {
	local sha; sha="$(master_sha)"
	header "release npm" "$([ "$execute" -eq 1 ] || echo '(dry run)')"
	kv "source" "packages/*/package.json @ $MASTER $(short "$sha")"
	kv "workflow" "$NPM_WF"
	local vp; vp="$(version_packages_pr)"
	[ -z "$vp" ] || warn "Version Packages PR #${vp%% *} is open; merge it first if you want the versions it carries: ${vp#* }"

	local d pv tagged skipped tags=() wanted=" ${args[*]:-} "
	while IFS=$'\t' read -r d pv tagged skipped; do
		[ -n "$d" ] || continue
		if [ ${#args[@]} -gt 0 ]; then case "$wanted" in *" $d "*) ;; *) continue ;; esac; fi
		if [ "$tagged" = 1 ]; then detail "$(printf '%-12s %-9s' "$d" "$pv") ${DIM}already tagged${RST}"
		elif [ "$skipped" = 1 ]; then detail "$(printf '%-12s %-9s' "$d" "$pv") ${DIM}skipped: $NPM_WF ignores this package${RST}"
		else detail "$(printf '%-12s %-9s' "$d" "$pv") → npm-$d-v$pv"; tags+=("npm-$d-v$pv"); fi
	done <<<"$(npm_packages)"
	[ ${#tags[@]} -gt 0 ] || { header "result"; kv "nothing" "every package version is tagged"; return; }

	steps_total=1
	step "tag ${#tags[@]} package(s) at $(short "$sha") and push, one push per tag"
	for t in "${tags[@]}"; do mutate git tag "$t" "$sha"; done
	push_tags "${tags[@]}"

	header "result"
	kv "runs" "$(actions_url "$NPM_WF")"
	kv "note" "infra-v3 apps pin @velocity-exchange/sdk explicitly; bump there if they need it"
	dry_run_kv
}

cmd_docker() {
	local sha; sha="$(master_sha)"
	header "release docker" "$([ "$execute" -eq 1 ] || echo '(dry run)')"
	kv "source" "$MASTER $(short "$sha")"
	kv "workflow" "$DOCKER_WF"
	kv "selection" "${args[*]:-changed (images with commits since their tag)}"

	local app lang path dtag dver dsha n next tags=() wanted=" ${args[*]:-} " known=""
	while IFS=$'\t' read -r app lang path; do
		[ -n "$app" ] || continue
		known="$known $app"
		dtag="$(latest_tag "docker-$app-")"; dver=""; dsha=""
		[ -z "$dtag" ] || { dver="$(tag_version "$dtag")"; dsha="$(tag_sha "$dtag")"; }
		# shellcheck disable=SC2046
		n="$(count_lines "$(commits_since "$dsha" $(docker_watch_paths "$lang" "$path"))")"
		if [ ${#args[@]} -gt 0 ]; then case "$wanted" in *" $app "*) ;; *) continue ;; esac
		elif [ "$n" -eq 0 ] && [ -n "$dtag" ]; then continue; fi
		next="$(next_patch "${dver:-0.1.-1}")"
		detail "$(printf '%-15s' "$app") v${dver:-none} @$(short "$dsha") → ${BOLD}v$next${RST}  ${DIM}$n commit(s) since${RST}"
		[ "$dsha" != "$sha" ] || warn "$app: last tag already points at $(short "$sha")"
		tags+=("docker-$app-v$next")
	done <<<"$(docker_apps)"
	for a in "${args[@]:-}"; do [ -z "$a" ] || case " $known " in *" $a "*) ;; *) die "unknown app \"$a\" (known:$known)" ;; esac; done
	[ ${#tags[@]} -gt 0 ] || { header "result"; kv "nothing" "no image has changes since its tag; name apps to force"; return; }

	steps_total=1
	step "tag at $(short "$sha") and push, one push per tag"
	for t in "${tags[@]}"; do mutate git tag "$t" "$sha"; done
	push_tags "${tags[@]}"

	header "result"
	kv "builds" "$(actions_url "$DOCKER_WF")"
	kv "then" "bash deploy-scripts/release.sh infra master   (once the builds are green; infra checks ECR)"
	dry_run_kv
}

cmd_infra() {
	[ -n "$infra_dir" ] || die "no infra checkout: pass --infra <path> or set VELOCITY_INFRA_DIR"
	[ -f "$infra_dir/scripts/deploy.js" ] || die "$infra_dir does not look like infrastructure-v3 (no scripts/deploy.js)"
	[ ${#args[@]} -gt 0 ] || die "which stage? $(infra_stages | cut -f1 | tr '\n' ' ')"
	local st row prod=0
	for st in "${args[@]}"; do
		row="$(stage_row "$st")"; [ -n "$row" ] || die "unknown stage \"$st\" (known: $(infra_stages | cut -f1 | tr '\n' ' '))"
		[ "$(printf '%s' "$row" | cut -f2)" != prod ] || prod=1
	done

	header "release infra ${args[*]}" "$([ "$execute" -eq 1 ] || echo '(dry run)')"
	kv "infra" "$infra_dir ${DIM}$(git -C "$infra_dir" rev-parse --abbrev-ref HEAD 2>/dev/null)${RST}"
	kv "pins read from" "origin/master (non-prod), origin/mainnet-beta (prod)"

	local app lang path latest pins cell line stale=()
	while IFS=$'\t' read -r app lang path; do
		[ -n "$app" ] || continue
		latest="$(tag_version "$(latest_tag "docker-$app-")")"; [ -n "$latest" ] || continue
		line=""; local behind=0
		for st in "${args[@]}"; do
			row="$(stage_row "$st")"
			pins="$(pinned_versions "$(printf '%s' "$row" | cut -f3)" "$(printf '%s' "$row" | cut -f4)" "$app" | tr '\n' '/' | sed 's,/$,,')"
			if [ -z "$pins" ]; then cell="${DIM}$st: not used${RST}"
			elif semver_gt "$latest" "${pins##*/}"; then cell="$st: ${YLW}$pins${RST}"; behind=1
			else cell="$st: ${GRN}$pins${RST}"; fi
			line="$line  $cell"
		done
		detail "$(printf '%-15s' "$app") latest v$latest $line"
		[ "$behind" -eq 0 ] || stale+=("$app")
	done <<<"$(docker_apps)"
	[ ${#stale[@]} -gt 0 ] || { header "result"; kv "nothing" "every stage already pins the latest published version"; return; }

	local dirty; dirty="$(git -C "$infra_dir" status --porcelain)"
	if [ -n "$dirty" ]; then
		[ "$execute" -eq 0 ] || die "infra working tree is dirty ($infra_dir); yarn deploy needs it clean"
		warn "infra working tree is dirty; yarn deploy will refuse until it is clean"
	fi
	note "yarn deploy resolves digests from ECR: run \`aws sso login --sso-session velocity\` first"

	local apps; apps="$(IFS=,; echo "${stale[*]}")"
	steps_total=1
	step "pin ${stale[*]} in ${args[*]}"
	unlock_signing_key "$infra_dir"
	mutate -C "$infra_dir" yarn deploy "$apps" "${args[@]}"

	header "result"
	kv "then" "merge the deploy PR yarn deploy printed; ArgoCD rolls the stage"
	[ "$prod" -eq 0 ] || kv "prod" "pins take effect only after the infra master → mainnet-beta PR: (cd $infra_dir && gh pr create --base mainnet-beta --head master)"
	dry_run_kv
}

[ "$do_fetch" -eq 0 ] || git fetch origin master --tags --force --quiet 2>/dev/null || warn "git fetch failed; using the local origin/master"
fetch_infra

case "$cmd" in
	status) cmd_status ;;
	bump) cmd_bump ;;
	devnet) cmd_devnet ;;
	npm) cmd_npm ;;
	docker) cmd_docker ;;
	infra) cmd_infra ;;
	mainnet) cmd_mainnet ;;
	*) usage >&2; die "unknown command: $cmd" ;;
esac
