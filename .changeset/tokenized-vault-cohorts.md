---
'@velocity-exchange/vaults-sdk': minor
---

A vault can now run several tokenized pools ("cohorts") at once, so a newcomer always has an at-par
pool to mint into.

A tokenized depositor holds one pooled cost basis for every holder of its mint, and the profit-share
fee comes out of the pool's own shares, so it dilutes every token equally. A pool is therefore only
fair to a newcomer while its value is at or above that basis, and once it has been under water it is
closed to new tokenizations until the vault recovers past the pooled high-water mark. Cohorts restore
availability: the manager opens a fresh pool with its own mint, its own shares and its own basis.

New program instruction `initialize_tokenized_vault_depositor_v2(params, cohort_id)`, which takes the
same accounts as the original plus a `cohort_id` argument in `1..=65535`. `TokenizedVaultDepositor`
gains a `cohortId` field, carved out of existing padding, so the account size is unchanged and
deployed pools need no migration.

Cohort 0 is the legacy pool. It keeps the exact addresses it always had, still routes through the
original instruction, and reads `cohortId === 0`. Every SDK entry point defaults to cohort 0, so
existing callers are unaffected:

- `getTokenizedVaultAddressSync` / `getTokenizedVaultMintAddressSync` take an optional trailing
  `cohortId`;
- `initializeTokenizedVaultDepositor` takes an optional `cohortId` in its params and routes to the v2
  instruction for ids 1 and above;
- `tokenizeShares` / `createTokenizeSharesIx` and `redeemTokens` / `createRedeemTokensIx` take an
  optional trailing `cohortId`;
- new `getTokenizedVaultDepositors`, `getTokenizedCohorts`, `findTokenizeableCohort` and
  `getNextCohortId` find the pool to mint into and the id for a new one (the first three need
  `getProgramAccounts`);
- new exports `MAX_TOKENIZED_COHORT_ID`, and the `TokenizedVaultDepositor` and `TokenizedCohort`
  types.

Tokens of different cohorts are separate SPL mints and are **not** fungible with each other. A holder
must redeem through the cohort whose mint they hold.

`tokenize_shares` no longer seed-checks its `mint` account, because a vault no longer has one
canonical mint. The pairing is enforced by `mint.key() == tokenized_vault_depositor.mint`, which
`redeem_tokens` has always relied on. Clients built from an older IDL keep working; the account is
the same account in the same position.
