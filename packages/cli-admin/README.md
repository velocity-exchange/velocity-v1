# @velocity-exchange/admin-cli

CLI for Velocity v1 admin operations. Sign with the right key (or pass a Squads
V4 multisig); the on-chain program enforces which tier of authority is
required for the action.

## Install

```sh
npm install -g @velocity-exchange/admin-cli
# or, one-off:
npx @velocity-exchange/admin-cli --help
```

## Usage

```sh
velocity-admin --help
```

## Profiles

Connection settings can be bundled into named profiles instead of repeated
flags, stored per-user at `~/.config/velocity-admin/config.json` (override
with `VELOCITY_ADMIN_CONFIG`). Multisig addresses live only in this local
config, deliberately not in the repo.

```sh
velocity-admin config init            # interactive; verifies everything against the live cluster
velocity-admin config list
velocity-admin config set-rpc <env> <url>   # shared per-cluster RPC, used by every profile without its own url
velocity-admin config set-default <name>
velocity-admin config remove <name>

velocity-admin -p mainnet-cold auth set-hot-admin accountExtension <pubkey>
VELOCITY_ADMIN_PROFILE=devnet velocity-admin extend-account --type state --dry-run
```

RPC URLs are shared per cluster (`rpcs` in the config): profiles normally
carry no url of their own and inherit the shared one for their env, so
rotating an RPC key is a single `config set-rpc`. A profile can still pin its
own url to deviate.

`config init` refuses to save anything it cannot verify: the RPC is
classified by genesis hash (never by its URL), the keypair must load, and a
multisig must exist on that cluster; its vault 0 is matched against the
live State admins and mismatches are called out.

Explicit flags always override the profile. Every command that touches the
chain prints a one-line context header (cluster · profile · signer · dispatch
mode), and dies when a declared env contradicts the RPC's actual genesis
hash. Mainnet direct sends ask for interactive confirmation; pass `--yes`
(implied when stdin is not a TTY) to skip.

## Commands

```
velocity-admin config init|list|set-rpc|set-default|remove   # connection profiles + shared RPCs (see Profiles below)
velocity-admin whoami                                # which on-chain authorities the signer holds

velocity-admin show config
velocity-admin show fees    # every fee users pay: trading tiers, filler reward, split, per-market adjustments + liquidation fees

velocity-admin auth set-cold-admin <pubkey>
velocity-admin auth set-warm-admin <pubkey>
velocity-admin auth set-pause-admin <pubkey>
velocity-admin auth set-hot-admin <role> <pubkey>
velocity-admin auth init-config [--initial-warm <pk>]

velocity-admin perp-market set-status <market> <status>
velocity-admin perp-market set-fee-buffer <market> <amount>
velocity-admin perp-market set-bankruptcy-if-floor <market> <pct>  # PERCENTAGE_PRECISION (1e6); 0 selects the 10 bps default, "disabled" turns the floor off
velocity-admin perp-market set-funding-dead-zone <market> <threshold> <slope>
velocity-admin perp-market set-oracle-slot-delay <market> <slots>
velocity-admin spot-market set-status <market> <status>
velocity-admin spot-market set-guard-threshold <market> <threshold>
velocity-admin spot-market set-scale-initial-asset-weight-start <market> <start>  # warm/cold admin; QUOTE_PRECISION (1e6); 0 disables
velocity-admin spot-market set-fee-factors <market> <ifFeeFactor> <protocolFeeFactor>
velocity-admin spot-market set-withdraw-breaker <market> <pct>
velocity-admin spot-market set-deposit-cap <market> <threshold> <pctPerDay>

velocity-admin exchange set-status <bitfield>
velocity-admin exchange sync-slot-duration <ms>        # permissionless; sync one IBRL transition (400->350->300->250->200) from its feature gate (previews the gate's activation/effective slots); effective slot derived onchain from the EpochSchedule
velocity-admin exchange set-solvency-status <bitfield>  # cold admin; gates solvency-repair ixs (1=solvencyRepairPaused)

velocity-admin feature-flags median-trigger-price <true|false>  # bit 2; enabling requires cold admin
velocity-admin feature-flags builder-codes <true|false>  # bit 4; enabling requires cold admin
velocity-admin feature-flags vamm-maker-rebate <true|false>  # bit 8; enabling requires cold admin

velocity-admin fees set-recipient <pubkey> <perp|spot>           # cold admin
velocity-admin fees set-split <ammFeeNumerator> <ifFeeNumerator> # warm/cold admin
velocity-admin fees set-taker-addon <market> <tenthBps>          # warm/cold admin; additive taker-fee add-on, -100..100 tenth-bps
velocity-admin fees set-promo-tier <tier>                        # warm/cold admin; promo fee-tier floor for everyone, 0 = off
velocity-admin fees set-referral-rate <percent>                  # warm/cold admin; Standard referrer reward on every active perp tier; Accelerated is a fixed constant
velocity-admin fees withdraw-perp <market> <amount>  # FeeWithdraw hot key; pays the recipient's ATA (created if needed)
velocity-admin fees withdraw-spot <market> <amount>  # FeeWithdraw hot key; pays the recipient's ATA (created if needed)
velocity-admin fees sweep <market>                               # permissionless
velocity-admin fees settle-revenue-share <market> [escrowAuthority] [--all] # permissionless; pays accrued builder/referrer fees out of the pnl pool. --all settles every escrow still owed on the market, which delisting now requires
velocity-admin fees transfer-fee-pnl <feePoolMarket> <pnlPoolMarket> <amount> <fee-to-pnl|pnl-to-fee> # warm/cold admin

velocity-admin user init <name> [--sub-accounts <n>] [--authority <pk>] [--vault-index <i>] [--dry-run]  # authority must sign on mainnet; one proposal with --multisig, vault pays rent
velocity-admin user set-delegate <delegate> [--sub-accounts <n>] [--allow-transfer <bool>] [--authority <pk>] [--vault-index <i>] [--dry-run]  # authority signs; one proposal with --multisig
velocity-admin user set-special-status <user> <flags>
velocity-admin user set-equity-floor <user> <floor> <buffer>     # warm/cold admin; QUOTE_PRECISION raw units; floor 0 disables both checks
velocity-admin user reset-equity-breaker <userStats>             # warm/cold admin; unfreezes an authority after the breaker tripped
velocity-admin user set-accelerated-referral <authority> <accelerated>  # warm/cold admin; grant clears the auto-enrollment block, revoke sets it
velocity-admin user equity-floor-status <authority>              # read-only; per-subaccount equity/floor/buffer/headroom + level + breaker flag
velocity-admin user close-positions [--sub-accounts <csv>]       # signer = account authority; cancel all orders + close all perp positions reduce-only
velocity-admin user admin-deposit <market> <amount> --user <pk> --user-token-account <pk>
velocity-admin user deposit <market> <amount> [--authority <pk>] [--vault-index <i>] [--sub-account <id>] [--user-token-account <pk>] [--reduce-only] [--dry-run]
velocity-admin user withdraw <market> <amount> [--authority <pk>] [--vault-index <i>] [--sub-account <id>] [--user-token-account <pk>] [--reduce-only] [--dry-run]

velocity-admin if stake <market> <amount> [--authority <pk>] [--user-token-account <pk>]  # inits the stake account if missing

velocity-admin wallet wrap-sol <lamports> [--authority <pk>] [--vault-index <i>] [--min-remaining <sol>] [--dry-run]  # wrap native SOL into the owner's wSOL ATA (created idempotently); one proposal with --multisig

velocity-admin program upgrade --buffer <pk> [--spill <pk>] [--dry-run]  # propose an upgrade from an existing on-chain buffer
velocity-admin program halt [--so <path>]                        # deploy sbpf-asm-abort + propose an upgrade that bricks the program
velocity-admin program close-buffers [--dry-run] [--program-only|--metadata-only]  # reclaim rent from orphaned program + IDL buffers

velocity-admin multisig create --proposer <pubkey> [--name <name>]  # create a Squads V4 1/1 multisig
velocity-admin multisig proposals [--limit <n>]                  # recent proposals: status, approvals, timelock ETA

velocity-admin extend-account <account>                          # AccountExtension hot key (or warm/cold); grow one zero-copy account to the deployed program's size
velocity-admin extend-account --type <type> [--batch-size <n>] [--dry-run]  # migration crank: scan + extend every account of a type (see docs/ACCOUNT-EXTENSION.md)

velocity-admin call <ixName> <payloadFile>     # generic IDL escape hatch
```

## Routing through a Squads V4 multisig

Append `--multisig <multisigPda>` to any subcommand. If the multisig's vault 0
PDA is a required signer of the action (e.g. it is the cold admin / authority),
the CLI submits a single transaction that creates a `vault_transaction` +
`proposal` against the multisig with your wallet as the proposer. Members then
approve + execute via the Squads UI. If the vault does **not** need to sign
(e.g. the wallet itself is the required authority), a proposal would be
pointless — the CLI says so and sends the transaction directly instead.

User-scoped commands (`user deposit`, `user withdraw`, `user set-delegate`,
`if stake`) default the authority to the multisig's vault 0 PDA when
`--multisig` is passed, since the vault is what signs at execution. The vault
must be the velocity user / stake authority and own the source token account.
`user deposit`, `user withdraw` and `user set-delegate` also honor
`--vault-index` to target and propose against a vault other than 0.

`user init` follows the same pattern on mainnet: the program only allows
account creation when the authority signs or is the payer, so with `--multisig`
the create instructions are batched into one proposal and the vault PDA is the
inner payer — the vault itself must hold enough SOL for the rent. Without
`--multisig` the local keypair is both authority and payer and the transaction
is sent directly.

```sh
velocity-admin auth set-warm-admin <newWarmAdmin> \
  --multisig <multisigPda> \
  --keypair ~/cold-proposer.json
```

## Creating a Squads V4 multisig

`multisig create` provisions a fresh Squads V4 multisig with the current wallet
as a 1/1 signer (full Initiate/Vote/Execute permissions) plus a proposer member
that can only Initiate transactions. The on-chain Squads program config supplies
the treasury; an ephemeral create-key seeds the multisig PDA.

```sh
velocity-admin multisig create \
  --proposer <proposerPubkey> \
  --name "Velocity Devnet Multisig" \
  --env devnet -u <devnetRpc>
```

## Generic dispatcher

For any velocity instruction without a dedicated wrapper:

```sh
velocity-admin call <camelCaseIxName> <payloadFile.json>
```

Example payload:

```json
{
	"args": { "withdrawGuardThreshold": "1000000000" },
	"accounts": {
		"spotMarket": "…",
		"state": "…",
		"adminAuthorityConfig": "…",
		"admin": "…"
	}
}
```

The dispatcher does no PDA derivation — every account must be supplied.

## Global options

| Flag                      | Default                                          |
| ------------------------- | ------------------------------------------------ |
| `-p, --profile <name>`    | `VELOCITY_ADMIN_PROFILE` env, else config default |
| `-u, --url <url>`         | profile, else `https://api.mainnet-beta.solana.com` |
| `-k, --keypair <path>`    | profile, else `~/.config/solana/id.json`         |
| `-e, --env <env>`         | profile, else detected from the RPC's genesis hash |
| `-m, --multisig <pubkey>` | profile, else none: direct send (`--no-multisig` forces direct under a proposing profile) |
| `-y, --yes`               | (unset; mainnet direct sends ask for confirmation) |

## Authority introspection

```sh
velocity-admin whoami [-p <profile>]      # which State roles the signer holds; multisig membership
velocity-admin multisig proposals [-m <pda>] [--limit <n>]  # recent proposals: status, approvals, timelock ETA
```

## Local development

```sh
cd cli-admin
bun install
bun run start --help    # run from src directly via bun
bun run build           # tsc → lib/
./lib/index.js --help   # run the compiled binary as the published package would
```

The committed `package.json` keeps `"@velocity-exchange/sdk": "file:../sdk"` so
local edits to the SDK are picked up immediately. Publishing rewrites that
to a real semver range based on `sdk/package.json`'s version (see
`scripts/prepare-publish.js`) and restores the `file:` ref afterwards. CI
handles this automatically; for a manual publish:

```sh
bun run publish-cli
```
