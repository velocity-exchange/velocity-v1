#!/bin/sh
# Shared helpers for devnet deploy scripts. Source with:
#   . "$(dirname "$0")/_lib.sh"
#
# Sets:
#   script_dir   absolute path to deploy-scripts/
#   repo_root    absolute path to repo root
#
# Defines:
#   drift_devnet_program_id   prints [programs.devnet].drift from Anchor.toml
#   resolve_drift_devnet_program_id <varname>
#                             sets <varname> to $<varname> if non-empty, else
#                             to the Anchor.toml value; errors if both are empty
#   resolve_upgrade_keypair <varname>
#                             sets <varname> from DRIFT_DEVNET_UPGRADE_KEYPAIR,
#                             or legacy SOLANA_PATH/$DEVNET_ADMIN; errors if neither

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

drift_devnet_program_id() {
	awk -F'"' '
		/^\[programs\.devnet\]/ { s=1; next }
		/^\[/                   { s=0 }
		s && $1 ~ /^drift[[:space:]]*=/ { print $2; exit }
	' "$repo_root/Anchor.toml"
}

resolve_drift_devnet_program_id() {
	# Resolve the devnet program id into the named variable.
	# Env override wins; otherwise read from Anchor.toml.
	_var="$1"
	eval "_cur=\${$_var:-}"
	if [ -z "$_cur" ]; then
		_cur="$(drift_devnet_program_id)"
	fi
	if [ -z "$_cur" ]; then
		echo "Could not resolve $_var from \$_var or Anchor.toml [programs.devnet].drift" >&2
		exit 1
	fi
	eval "$_var=\"\$_cur\""
}

resolve_upgrade_keypair() {
	_var="$1"
	if [ -n "${DRIFT_DEVNET_UPGRADE_KEYPAIR:-}" ]; then
		eval "$_var=\"\$DRIFT_DEVNET_UPGRADE_KEYPAIR\""
	elif [ -n "${SOLANA_PATH:-}" ] && [ -n "${DEVNET_ADMIN:-}" ]; then
		eval "$_var=\"\$SOLANA_PATH/\$DEVNET_ADMIN\""
	else
		echo "Set DRIFT_DEVNET_UPGRADE_KEYPAIR (recommended), or both SOLANA_PATH and DEVNET_ADMIN" >&2
		exit 1
	fi
}
