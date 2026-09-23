#!/bin/sh
# Deploy the jit-proxy program to its vanity address on devnet or mainnet.
#
#   bash deploy-scripts/deploy-jit-proxy.sh <devnet|mainnet>
#
# The program id (J1TPRoX…, Anchor.toml [programs.devnet].jit_proxy) is a
# create-with-seed address (ground with cavemanloverboy/vanity): there is no
# keypair for it, so `solana program deploy` cannot perform the initial deploy
# (it requires the program account to sign). Instead this script:
#   1. builds target/deploy/jit_proxy.so (skip with SKIP_BUILD=1) — the same
#      .so serves both clusters: jit-proxy has no mainnet-beta feature and pins
#      its velocity dep to default-features = false, features = ["cpi"]
#   2. writes it into a buffer via `solana program write-buffer` with a
#      persisted per-cluster buffer keypair (re-run to resume a partial upload)
#   3. runs deploy-jit-proxy.ts, which creates the program account with
#      SystemProgram.createAccountWithSeed (signed by the base keypair) and
#      sends the loader's DeployWithMaxDataLen instruction directly
# Once deployed, future upgrades are ordinary buffer upgrades — this script is
# only for the initial deploy (it exits if the program already exists).
#
# Env:
#   RPC_URL / SOLANA_RPC   RPC endpoint; REQUIRED for mainnet, defaults to
#                          https://api.devnet.solana.com on devnet (use a
#                          private RPC — public endpoints rate-limit the
#                          chunked buffer writes)
#   DEPLOYER_KEYPAIR       payer + buffer/upgrade authority
#                          (default ~/.config/solana/id.json)
#   BASE_KEYPAIR           keypair of the create-with-seed base pubkey
#                          Fqo8WncpiP55ExPhuWLzMbm1cuYuJpXjhQPnd8ak3Pyn; only
#                          needed while the program account doesn't exist yet
#   BUFFER_ACCOUNT_KEYPAIR default deploy-scripts/out/jit-proxy-so-write-buffer-<cluster>-keypair.json
#   PROGRAM_SO             default target/deploy/jit_proxy.so
#   MAX_DATA_LEN           programdata capacity (default: size of the .so;
#                          upgrades auto-extend, so headroom is optional)
#   SKIP_BUILD=1           reuse an existing $PROGRAM_SO
#   NON_INTERACTIVE=1 / YES=1  skip the confirmation prompt

set -eu

. "$(dirname "$0")/_lib.sh"

CLUSTER="${1:-}"
case "$CLUSTER" in
	devnet) DEFAULT_RPC="https://api.devnet.solana.com" ;;
	mainnet) DEFAULT_RPC="" ;;
	*) echo "usage: $0 <devnet|mainnet>" >&2; exit 1 ;;
esac

SOLANA_RPC="${SOLANA_RPC:-${RPC_URL:-$DEFAULT_RPC}}"
if [ -z "$SOLANA_RPC" ]; then
	echo "Set RPC_URL (or SOLANA_RPC) — refusing to write a mainnet buffer through a default public endpoint" >&2
	exit 1
fi

DEPLOYER_KEYPAIR="${DEPLOYER_KEYPAIR:-$HOME/.config/solana/id.json}"
PROGRAM_SO="${PROGRAM_SO:-target/deploy/jit_proxy.so}"
BUFFER_ACCOUNT_KEYPAIR="${BUFFER_ACCOUNT_KEYPAIR:-deploy-scripts/out/jit-proxy-so-write-buffer-$CLUSTER-keypair.json}"

cd "$repo_root"

if [ "${SKIP_BUILD:-}" != "1" ]; then
	bash deploy-scripts/build-sbf.sh mainnet jit-proxy
fi

if [ ! -f "$PROGRAM_SO" ]; then
	echo "Missing $PROGRAM_SO — run without SKIP_BUILD=1" >&2
	exit 1
fi

# Refuse to upload anything that isn't the deployable bytecode version, including
# when SKIP_BUILD=1 hands us an artifact somebody else built.
bash deploy-scripts/assert-sbpf-version.sh "$PROGRAM_SO"

mkdir -p "$(dirname "$BUFFER_ACCOUNT_KEYPAIR")"
if [ ! -f "$BUFFER_ACCOUNT_KEYPAIR" ]; then
	solana-keygen new --no-bip39-passphrase -s -o "$BUFFER_ACCOUNT_KEYPAIR"
fi

# Resumable: re-running only re-sends chunks that didn't land.
solana program write-buffer "$PROGRAM_SO" \
	-u "$SOLANA_RPC" \
	--buffer "$BUFFER_ACCOUNT_KEYPAIR" \
	--buffer-authority "$DEPLOYER_KEYPAIR" \
	--fee-payer "$DEPLOYER_KEYPAIR" \
	-k "$DEPLOYER_KEYPAIR"

BUFFER_PK=$(solana-keygen pubkey "$BUFFER_ACCOUNT_KEYPAIR")

CLUSTER="$CLUSTER" \
RPC_URL="$SOLANA_RPC" \
DEPLOYER_KEYPAIR="$DEPLOYER_KEYPAIR" \
BUFFER_PUBKEY="$BUFFER_PK" \
	bun run "$script_dir/deploy-jit-proxy.ts"

PROGRAM_ID=$(awk -F'"' '/^\[programs\.devnet\]/{s=1;next}/^\[/{s=0}s && $1 ~ /^jit_proxy[[:space:]]*=/{print $2;exit}' "$repo_root/Anchor.toml")
echo ""
echo "Next: create the canonical IDL metadata account (once per cluster, while the"
echo "deployer still holds the upgrade authority — see deploy-scripts/README.md):"
echo "  bunx @solana-program/program-metadata@0.5.1 create idl $PROGRAM_ID \\"
echo "    packages/jit-proxy/src/idl/jit_proxy.json --keypair $DEPLOYER_KEYPAIR --rpc $SOLANA_RPC"
