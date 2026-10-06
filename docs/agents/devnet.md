# Devnet program upgrade and wipe

Read this before any devnet upgrade, wipe, or reinit. The full runbook is in
[`../../deploy-scripts/README.md`](../../deploy-scripts/README.md). Read its "Operational notes"
section first. This file lists the rules and the pitfalls that have already cost someone time.

## Upgrade rules

- Always use a private RPC for `solana program` and `anchor program upgrade` writes. velocity.so is
  around 5 MB, which is roughly 5,000 chunked writes, and `api.devnet.solana.com` rate-limits the
  upload partway through every time. Also run `solana config set --url <url>` so the underlying
  CLI inherits it.
- Prefer the two-phase deploy over `anchor program upgrade`. Run
  `deploy-scripts/write-buffer-devnet.sh`, which creates or resumes a named onchain buffer, and then
  `deploy-scripts/deploy-from-buffer-devnet.sh`, which swaps it in with one transaction.
  `anchor program upgrade` creates an anonymous buffer and closes it on failure, so the next retry
  restarts from chunk 0. The two-phase flow keeps the buffer pubkey on disk, so re-running
  `write-buffer-devnet.sh` only re-sends the chunks that didn't land.
- Resume until done. `write-buffer` can exit 0 with the buffer still partial. Check with
  `solana program show <BUFFER_PK>`. Data Length must be at least the `.so` size. If
  `deploy-from-buffer` fails with `Failed to parse ELF file: invalid section header` or
  `invalid account data for instruction`, the buffer is partial. Re-run `write-buffer-devnet.sh`
  against the same buffer keypair and try again.
- Reclaim rent from orphaned buffers (about 38 SOL each for velocity-sized buffers). List them with
  `solana program show --buffers [--buffer-authority <pk>]`, and close all buffers under one
  authority with `solana program close --buffers --recipient <pk> --buffer-authority <keypair>`.
  Check both the CLI default keypair and the upgrade-authority keypair as candidate authorities.
- Anchor 1.0 renamed `anchor upgrade` to `anchor program upgrade`. `deploy-devnet.sh` uses the new
  form.

After a successful upgrade with a layout-breaking change, run `deploy-scripts/wipe-devnet.ts`
(calls the devnet-only `force_wipe_accounts_devnet` instruction) and then
`deploy-scripts/init-devnet.sh` to recreate state under the new layouts.

## Wipe and reinit pitfalls

- SPL token vaults survive a velocity-only wipe. Only the owning program can decrement an account's
  lamports. `force_wipe_accounts_devnet` zeroes velocity-owned PDAs but cannot touch
  `spot_market_vault` or `insurance_fund_vault`, which the Token program owns. After a wipe these
  vaults remain, and `initialize_spot_market` then fails with
  `Allocate: account ... already in use`, because Anchor's `init` constraint always calls System
  Allocate on the same PDA address.
- Closing an SPL token account requires `amount == 0`. The Token program rejects `close_account`
  with `Non-native account can only be closed if its balance is zero` (error `0xb`). The wipe
  instruction must `spl_token::burn` or transfer before closing, and `burn` needs the mint passed
  as a writable account. `wipe-devnet.ts` reads each vault's data onchain to find its mint and
  passes `(vault, mint)` pairs in `remaining_accounts`.
- Mixing manual lamport mutation with CPI in one loop trips the runtime. Solana's per-CPI
  conservation check fails with
  `sum of account balances before and after instruction do not match` if you manually credit admin
  lamports and then CPI into another program that also
  rebalances lamports. Do all CPI closes in one pass, then all manual drains in a second pass.
- The IDL regen recipe must drop `mainnet-beta`, or devnet-only instructions vanish from the IDL.
  Default features include `mainnet-beta`, which strips `#[cfg(not(feature = "mainnet-beta"))]`
  items. The deployed `.so` has the instruction (built by `build-devnet.sh` with
  `--no-default-features`), but `program.methods.forceWipeAccountsDevnet` is undefined in the SDK
  because the IDL doesn't list it. Regenerate with `bun run program:idl`.
- Anchor 1.0 `.accounts()` behaves like `accountsPartial` and may reorder. When sending a wipe
  instruction with explicit `velocitySigner` and `tokenProgram`, the auto-resolver can shift them
  into `remaining_accounts`. Use `.accountsStrict({...})` for fixed account sets.
- `deploy-from-buffer-devnet.sh` insists on a `PROGRAM_KEYPAIR` file. An upgrade needs only the
  upgrade authority, not the program keypair. Run it directly:
  `solana program deploy target/deploy/velocity.so --buffer <BUF_PK> --program-id <PROGRAM_PUBKEY> --upgrade-authority <KP> -u <URL>`
  (`--program-id` accepts a pubkey for upgrades).
- `wipe-devnet.ts` also walks the `.wiped-*.json` archives, not just the active receipt. Every
  spot-market index ever recorded gets its derived vault PDAs included in later wipes. Don't
  delete the archives until you are certain no onchain accounts remain.
- Phase G (LP pool) creates more orphan token accounts. The LP-pool subaccounts (for example the
  dUSDT constituent token vault) survive a wipe the same way spot vaults do. If you don't need an
  LP pool, set `SKIP_PHASE_G=1`. Otherwise extend the `wipe-devnet.ts` collector to derive the
  LP-pool vault PDAs.
- Removing a feature breaks the deploy scripts. PR #38's PMM removal is the example. The SDK
  exports that `init-devnet.ts` imported disappeared, so the script crashed at import. After any
  feature removal, search `deploy-scripts/` for helpers named after that feature and remove the
  phase before the next devnet run.
