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

## Commands

```
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
velocity-admin exchange set-solvency-status <bitfield>  # cold admin; gates solvency-repair ixs (1=solvencyRepairPaused)

velocity-admin feature-flags median-trigger-price <true|false>  # bit 2; enabling requires cold admin
velocity-admin feature-flags builder-codes <true|false>  # bit 4; enabling requires cold admin
velocity-admin feature-flags vamm-maker-rebate <true|false>  # bit 8; enabling requires cold admin

velocity-admin fees set-recipient <pubkey> <perp|spot>           # cold admin
velocity-admin fees set-split <ammFeeNumerator> <ifFeeNumerator> # warm/cold admin
velocity-admin fees set-transaction-rails <inclusionLamports> <signatureLamports> <resourceFeeNum> <resourceFeeDenom>  # warm/cold admin; what a transaction costs to land. Every relay crank payment is derived from it, so this re-prices them all; markets take the new figures on their next set-market-clob
velocity-admin fees set-liquidation-crank-reimbursement <shareBps> <solSpotMarketIndex>  # cap on what the protocol repays a liquidation cranker (its priority fee, bounded by a share of the recovery), and the market pricing it in SOL; warm/cold admin
velocity-admin fees init-crank-treasury                                # warm/cold admin; create the one account every market's crank reservoir refills from. Run once, then fund it by sending SOL to the printed address
velocity-admin fees set-crank-treasury <refillTargetCranks> <refillWatermarkCranks>  # warm/cold admin; the two levels a market's crank reservoir is held between, counted in that market's dearest crank so one setting fits every market. The watermark is when a refill wakes and must cover the refill's own round trip; the target is how full it leaves the reservoir and must exceed it. A new watermark reaches a market on its next set-market-clob
velocity-admin fees withdraw-crank-treasury <lamports>                 # warm/cold admin; recover lamports from the crank treasury, never below rent
velocity-admin fees sweep-crank-reservoir <marketIndex> <lamports>     # warm/cold admin; move lamports from a market's reservoir back to the treasury (retired or over-provisioned markets)
velocity-admin fees set-taker-addon <market> <tenthBps>          # warm/cold admin; additive taker-fee add-on, -100..100 tenth-bps
velocity-admin fees set-promo-tier <tier>                        # warm/cold admin; promo fee-tier floor for everyone, 0 = off
velocity-admin fees withdraw-perp <market> <amount>  # FeeWithdraw hot key; pays the recipient's ATA (created if needed)
velocity-admin fees withdraw-spot <market> <amount>  # FeeWithdraw hot key; pays the recipient's ATA (created if needed)
velocity-admin fees withdraw-protocol-user <market> <amount>  # FeeWithdraw hot key; drains settled crank rewards from the protocol-owned User (settle-pnl first)
velocity-admin fees sweep <market>                               # permissionless
velocity-admin fees settle-revenue-share <market> [escrowAuthority] [--all] # permissionless; pays accrued builder/referrer fees out of the pnl pool. --all settles every escrow still owed on the market, which delisting now requires
velocity-admin fees transfer-fee-pnl <feePoolMarket> <pnlPoolMarket> <amount> <fee-to-pnl|pnl-to-fee> # warm/cold admin

velocity-admin user init <name> [--sub-accounts <n>] [--authority <pk>] [--vault-index <i>] [--dry-run]  # authority must sign on mainnet; one proposal with --multisig, vault pays rent
velocity-admin user set-delegate <delegate> [--sub-accounts <n>] [--allow-transfer <bool>] [--authority <pk>] [--vault-index <i>] [--dry-run]  # authority signs; one proposal with --multisig
velocity-admin user set-special-status <user> <flags>
velocity-admin user set-equity-floor <user> <floor> <buffer>     # warm/cold admin; QUOTE_PRECISION raw units; floor 0 disables both checks
velocity-admin user reset-equity-breaker <userStats>             # warm/cold admin; unfreezes an authority after the breaker tripped
velocity-admin user equity-floor-status <authority>              # read-only; per-subaccount equity/floor/buffer/headroom + level + breaker flag
velocity-admin user close-positions [--sub-accounts <csv>]       # signer = account authority; cancel all orders + close all perp positions reduce-only
velocity-admin user admin-deposit <market> <amount> --user <pk> --user-token-account <pk>
velocity-admin user deposit <market> <amount> [--authority <pk>] [--vault-index <i>] [--sub-account <id>] [--user-token-account <pk>] [--reduce-only] [--dry-run]
velocity-admin user withdraw <market> <amount> [--authority <pk>] [--vault-index <i>] [--sub-account <id>] [--user-token-account <pk>] [--reduce-only] [--dry-run]

velocity-admin quoter init <market> <quoterProgram> <user> <responseAccount> <quoteDisc> <executeDisc> [--type <vamm|clob|custom>] [--authority <pk>]  # born active but unapproved; custom entries must be created by the quoted user's authority; discs = 16 hex chars
velocity-admin quoter update-accounts <quoter> <quote|execute> <index> <metas...> [--authority <pk>]  # meta = "<pubkey>" or "<pubkey>:w"; clears admin approval
velocity-admin quoter update-config <quoter> [--response-account <pk>] [--quote-disc <hex>] [--execute-disc <hex>] [--authority <pk>]  # clears admin approval
velocity-admin quoter set-active <quoter> <true|false> [--authority <pk>]  # maker kill switch; entry authority signs
velocity-admin quoter set-approved <quoter> <true|false> [--admin <pk>]    # warm/cold admin vetting gate
velocity-admin quoter set-priority <quoter> <0-255> [--admin <pk>]         # warm/cold admin; lower fills first, pro rata within a tier
velocity-admin quoter set-market-clob <market> <quoter> <clobMarket> [expireFallbackSlots] [--crank-cu <n>] [--crank-cu-<crank> <n>] [--admin <pk>]  # warm/cold admin; names the mandatory-baseline CLOB and stands up (or re-prices) the market's relay crank conditions + reservoir. Each crank's keeper payment is derived from the cost units it requests and the fee rails
velocity-admin quoter set-watch <quoter> --watch-account <pk> --offset <n> --len <n> [-a <pk>]  # entry authority; declares the reprice region relay cross-discovery wakes on (len 0 clears); resets approval
velocity-admin quoter attach-cross <quoter> [--fallback-slots <n>]           # permissionless; stands up (or re-prices) the entry's relay cross-discovery conditions

velocity-admin clob-market init <market> --clob-program <pk> [--capacity <n>] [--crank-cu <n>] [--crank-cu-<crank> <n>] [--relay-program <pk>|none] [book config flags]  # one-shot bring-up: book create+init, quoter register+approve, canonical attach (creates crank conditions), relay watches (both blocks); warm/cold admin, direct-send only
velocity-admin clob-market register-watch <market> [--relay-program <pk>]  # register a relay WatchV0 over BOTH of an existing market's condition blocks (velocity's conditions account and the book's own); permissionless, direct-send only

> **Turner scoping.** A market's conditions live on two accounts: velocity's
> crank-conditions PDA (the cross fallback poll) and the CLOB market itself
> (expiry, activation, a side at its eviction threshold, a crossed book — all
> facts about the book's own account, kept current by the book). Both get a
> relay watch, and a turner must allow both programs:
> `--target-program <velocity-id>,<clob-id>`. Allowing only velocity filters
> the book's watches out at the registry query, and none of its cranks fire.


velocity-admin if stake <market> <amount> [--authority <pk>] [--user-token-account <pk>]  # inits the stake account if missing

velocity-admin program upgrade --buffer <pk> [--spill <pk>] [--dry-run]  # propose an upgrade from an existing on-chain buffer
velocity-admin program halt [--so <path>]                        # deploy sbpf-asm-abort + propose an upgrade that bricks the program
velocity-admin program close-buffers [--dry-run] [--program-only|--metadata-only]  # reclaim rent from orphaned program + IDL buffers

velocity-admin multisig create --proposer <pubkey> [--name <name>]  # create a Squads V4 1/1 multisig

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

| Flag                      | Default                               |
| ------------------------- | ------------------------------------- |
| `-u, --url <url>`         | `https://api.mainnet-beta.solana.com` |
| `-k, --keypair <path>`    | `~/.config/solana/id.json`            |
| `-e, --env <env>`         | `mainnet-beta` (or `devnet`)          |
| `-m, --multisig <pubkey>` | (none — direct send)                  |

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
