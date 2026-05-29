# Spike: unify referrer + builder revenue-share onto one rail

**Status:** proof-of-concept. Goal is to show the unification is _feasible_ and small, not to ship a complete migration.

## Thesis

The protocol currently has **two parallel fee-share payout rails**:

1. **Referrer (legacy):** identity in `UserStats.referrer`; the reward is credited
   _inline on the fill_ to the referrer's perp quote position and tallied in
   `UserStats.fees` — which forces the referrer's **User + UserStats** accounts
   to be loaded on every referred fill.
2. **Builder codes (new):** identity in the taker's `RevenueShareEscrow`
   (`approved_builders`); the fee _accrues_ into a `RevenueShareOrder.fees_accrued`
   slot on the fill and _settles later_ via `settle_pnl` into the recipient's
   `RevenueShare` account. The fill touches only the **taker's escrow**.

These are redundant. The builder rail is the better primitive, and the codebase
was already built to host the referrer on it — it just hasn't cut over.

## Evidence the rail is already shared (no new design needed)

All in `programs/drift/src/state/revenue_share.rs`:

- `RevenueShareEscrow.referrer: Pubkey` — a referrer slot already lives on the escrow.
- `RevenueShareOrderBitFlag::Referral` — referral is a first-class order kind.
- `RevenueShareEscrow::find_or_create_referral_index(market, market_type)` —
  auto-provisions a referral accrual slot.
- `RevenueShare { total_referrer_rewards, total_builder_rewards, .. }` — one
  settlement account already counts **both** kinds.
- `migrate_referrer` instruction — backfills `escrow.referrer` from `UserStats.referrer`.

And the fill path already prefers the escrow when present, falling back to the
legacy inline credit — i.e. it is mid-migration:

`programs/drift/src/controller/orders.rs`

- `fulfill_perp_order_with_amm` — `if reward_referrer { escrow slot } else { legacy inline }`
- `fulfill_perp_order_with_match` — the same dual branch

## The unified model

Treat both as **one primitive — a fee-share recipient** — and keep a thin policy
layer for the two genuine differences.

**Unified (mechanism):**

- One identity store: the taker's `RevenueShareEscrow` (`escrow.referrer`).
- One accrual: every share lands in a `RevenueShareOrder.fees_accrued` slot.
- One settlement: `settle_pnl` → recipient's `RevenueShare`.

**Parameterized (policy — keep distinct):**

1. **Referee discount** — referrer-only, no builder analog. Stays a property of the referral order.
2. **Rate source** — builder = user-chosen (≤ `max_fee_tenth_bps`); referral = protocol param, stamped at order creation.
3. **Stickiness** — builder = explicit per-order `builder_idx`; referral = sticky, auto-applied (`find_or_create_referral_index`).

In one line: **a referrer is an implicit, protocol-rated, sticky builder that
also grants the taker a discount.**

## What this spike changes

Collapse the dual branch in both perp fill methods to the **escrow-only** rail,
deleting the legacy inline referrer credit:

```rust
if reward_referrer {
    if let (Some(idx), Some(escrow)) = (referrer_builder_order_idx, rev_share_escrow.as_mut()) {
        let order = escrow.get_order_mut(idx)?;
        order.fees_accrued = order.fees_accrued.safe_add(referrer_reward)?;
    }
}
```

That single move is what lets the referrer's User + UserStats drop off the perp
fill path — the same outcome the standalone `RevShareClaim` + global vault design
in `plan-userstats-fill-removal.md` was invented to achieve, but reusing an
existing on-chain rail instead of adding a vault, a claim PDA, and two hot-role
instructions.

### Why this is strictly better than the vault design

- **Achieves the fill-path goal directly** — accrual is into the _taker's_ escrow
  slot, so the referrer accounts are never needed on the fill.
- **Stronger trust model** — accrual is fully on-chain (`fees_accrued`); no
  offchain attribution and no hot-role that can re-shuffle accrued rewards
  (the weakest part of the vault plan's trust section).
- **Fewer moving parts** — reuse `RevenueShare` / `settle_pnl` instead of a new
  vault + `RevShareClaim` + `set_rev_share_claimable`.

Trade-off (honest): the escrow rail is **pull** (settle per-market via the PnL
pool, user-triggered) vs. the vault's **push** (claim anytime), and it keeps more
per-fill on-chain state. For _removing UserStats from fills_, the escrow already
does the job.

## Migration prerequisite (the gap this spike intentionally leaves)

Removing the legacy inline path means a referred user with **no escrow** earns
their referrer nothing on-fill. Production cutover therefore needs:

1. Ensure every referred user has a `RevenueShareEscrow` with `referrer` set
   (`migrate_referrer` already does the backfill).
2. Guarantee a referral order slot is provisioned for active referred markets
   (`find_or_create_referral_index`).
3. Only then delete the legacy branch protocol-wide (this PR).

## Remaining work beyond the spike

- **Account context / loaders:** drop referrer `User`/`UserStats` from the perp
  fill `remaining_accounts` plumbing (`get_referrer_info`, `load_user_maps`) once
  the legacy path is gone everywhere.
- **Referee discount:** today still tallied in `UserStats.fees.total_referee_discount`
  (`orders.rs`, `increment_total_referee_discount`). Decide: keep as a
  pure fee reduction (no UserStats write) or move the tally onto the escrow.
- **Epoch cap:** `current_epoch_referrer_reward` has no escrow analog — drop the
  on-chain cap (matches "move volume/caps offchain") or re-implement on the escrow.
- **Tests:** unit test asserting referral reward lands in `fees_accrued` and
  settles into `RevenueShare.total_referrer_rewards`; integration test for a
  referred perp fill with no referrer accounts in the tx.
- **IDL + SDK:** regenerate and update fill builders to stop attaching referrer
  accounts.
