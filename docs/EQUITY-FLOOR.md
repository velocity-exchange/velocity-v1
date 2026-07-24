# Equity floor

How the equity floor on delegated accounts works: what it enforces, what happens when it trips, and
how to move funds between subaccounts without tripping it.

This applies to accounts that Velocity creates and funds under its own authority, with the borrowing
maker's trading key set as the `delegate` on each subaccount. The loan is deposited in USDT across
one or more subaccounts, and each subaccount carries an `equity_floor`: a minimum account equity,
denominated in USDT, that the subaccount must stay above. Velocity sets the floors when the accounts
are funded; the standard arrangement is 70% of the loan amount, i.e. a maximum loss of 30% of the
loan. On a 1,000,000 USDT loan, the floors across the subaccounts sum to 700,000 USDT.

"Equity" here is the subaccount's cross-margin total collateral: deposits plus unrealized PnL,
valued at oracle prices (the withdraw and transfer paths use strict, TWAP-bounded oracle pricing).
The floor is stored on the `User` account in `QUOTE_PRECISION` (1e6), so a 700,000 USDT floor is
`700_000_000_000`. A floor of `0` means the check is disabled. Only Velocity's admin can set or
change the total floor; what the delegate controls is how the floor is split across subaccounts (see
[Moving funds between subaccounts](#moving-funds-between-subaccounts)).

## What it enforces day to day

While a subaccount's equity is at or above its floor, the floor has no effect. It is only checked on
actions that could push equity below it:

| Action                                                          | Check                                                    |
| --------------------------------------------------------------- | -------------------------------------------------------- |
| Risk-increasing order placement and fills (taker or maker side) | Rejected if equity is below the floor                     |
| Withdrawals                                                     | Rejected if equity would end below the floor              |
| Transfers out of a subaccount (deposits, perp positions, pools) | Rejected if the debited side would end below the floor    |
| Swaps                                                           | Rejected if equity would end below the floor              |
| Reduce-only orders, closing positions, deposits, settles        | Always allowed, floor or no floor                         |

All of these revert with `EquityBelowFloor` (error code 6358). The floor does not restrict
de-risking; it only blocks adding risk or withdrawing funds while equity is at or under the agreed
minimum.

## The breaker

The checks above only apply to actions. Equity can also fall below the floor through trading losses;
the breaker covers that case.

`tripEquityFloorBreaker` is a permissionless instruction: any keeper can call it against a
subaccount, and the on-chain proof is simply a margin calculation showing that subaccount's equity
is below its floor. Velocity runs a guard bot that watches every floored account, so once any
subaccount drops below its floor, expect the breaker to be tripped within seconds.

Tripping sets the `equityBreakerTripped` flag on the authority's `UserStats` account. This flag is
authority-wide: it freezes every subaccount under the authority, not just the one that breached.
While it is set, all subaccounts reject:

- risk-increasing fills (both as taker and as maker; resting risk-increasing trigger orders are
  cancelled instead of triggered),
- withdrawals,
- transfers out (deposit transfers, perp position transfers, pool transfers),
- swaps.

Reduce-only activity remains allowed: the delegate can still close positions, cancel orders,
deposit, and settle PnL. The accounts are not liquidated or seized.

The flag does not clear itself, even if equity recovers above the floor. Only Velocity's warm admin
can clear it, via `resetEquityFloorBreaker`, after a human has reviewed why it fired. If the breaker
trips, contact Velocity.

## Moving funds between subaccounts

The floor is checked per subaccount, and each subaccount's floor is its own field. Moving USDT from
subaccount A to subaccount B without also moving floor leaves A clearing its full original floor
with less collateral in it. Rebalancing should therefore move the floor along with the funds.

`transferDepositByDelegate` takes an `equityFloorDelta` parameter for exactly this: it shifts that
much floor from the debited subaccount to the credited one, atomically, in the same instruction as
the funds. The sum of floors across the subaccounts never changes; only the split does. Three rules
are enforced on-chain, and violating any of them reverts the whole transfer with
`InvalidEquityFloorTransfer` (error code 6359):

1. The debited side must not already be below its floor. A subaccount that is already below its
   floor cannot move floor away to avoid a pending breaker trip.
2. The debited side must end at or above its reduced floor after the funds leave (this is the normal
   withdraw-side check, against the new, lower floor).
3. The credited side must end at or above its increased floor after the funds land, so the increased
   floor is backed by actual equity.

```ts
// move 250k USDT from subaccount 0 to subaccount 1, carrying 50k of floor with it
await velocityClient.transferDepositByDelegate(
	new BN(250_000).mul(QUOTE_PRECISION), // amount, token precision (USDT = 1e6)
	0, // marketIndex: USDT
	0, // fromSubAccountId
	1, // toSubAccountId
	new BN(50_000).mul(QUOTE_PRECISION) // equityFloorDelta, or 'auto'
);
```

To rebalance floor without moving funds (e.g. after PnL has shifted where the equity sits), pass a
zero `amount` with an explicit `equityFloorDelta`. The same three rules apply.

## `'auto'` mode

The SDK can compute the delta: pass `'auto'` as `equityFloorDelta` and it resolves to the minimal
floor that must move for the debited side to stay at or above its floor:

```
excess = max(0, collateral_from - floor_from)   // equity the from side holds above its floor
delta  = min(max(amount - excess, 0), floor_from)
```

The first `excess` USDT of the transfer is equity held above the floor and carries no floor with it;
anything beyond that must take floor along, capped at the floor the debited side has. Worked
example, from side holding 500k USDT of equity against a 350k floor, transferring 250k:

```
excess = 500k - 350k            = 150k
delta  = min(250k - 150k, 350k) = 100k
```

After the transfer the from side holds 250k of equity against a 250k floor (exactly at its floor),
and the to side gains 250k of equity and 100k of floor. The delta never exceeds `amount`, so the
credited side stays backed whenever it was backed before.

Two caveats:

- `'auto'` is quote market (USDT) only. It assumes the amount and equity share `QUOTE_PRECISION`;
  for any other spot market, the caller must value the tokens and pass an explicit delta.
- The SDK prices collateral client-side with the same strict oracle rules as the program, but at the
  exact boundary the two can disagree by dust. If an `'auto'` transfer reverts, retry with an
  explicit, slightly padded delta.

## The invariant

All of the checks above enforce one rule:

> Every subaccount's equity must cover its floor, and the floors always sum to the agreed total.

The program pins the sum (only the transfer instruction can move floor, and it conserves it; only
Velocity's admin can change the total), and checks equity against floor per subaccount on every
risk-increasing action. The aggregate consequence is that total equity across the subaccounts always
covers the agreed total floor, e.g. 70% of the loan.

As long as the rule holds, the delegate can split the loan across subaccounts and move funds and
floor between them freely. If any subaccount falls below its floor, the breaker trips and all
subaccounts become reduce-only until a Velocity admin resets the flag.

## Monitoring

The SDK mirrors the on-chain checks:

| Helper                                  | What it reports                                                               |
| --------------------------------------- | ----------------------------------------------------------------------------- |
| `user.isBelowEquityFloor(strict)`       | `true` when this subaccount's equity is below its floor (trip condition)      |
| `user.getEquityAboveFloor(strict)`      | Equity headroom above the floor, floored at zero; `null` when no floor is set |
| `userAccount.equityFloor`               | The subaccount's floor, `QUOTE_PRECISION`                                     |
| `userStatsAccount.equityBreakerTripped` | Whether the authority-wide breaker is currently set                           |

Pass `strict = true` to match the TWAP-bounded pricing the withdraw/transfer paths use. Alerts
should fire well before headroom reaches zero; the breaker is permissionless, so Velocity's guard
bot is not the only party that can call it.

## Quick reference

| Instruction                 | Who can call        | What it does                                                          |
| --------------------------- | ------------------- | --------------------------------------------------------------------- |
| `updateUserEquityFloor`     | Velocity admin      | Sets a subaccount's floor (changes the total)                          |
| `transferDepositByDelegate` | The delegate        | Moves funds and floor between subaccounts, conserving the floor sum    |
| `tripEquityFloorBreaker`    | Anyone              | Proves one subaccount is below its floor, freezes the whole authority  |
| `resetEquityFloorBreaker`   | Velocity warm admin | Clears the breaker after review                                        |

| Error                        | Code | Meaning                                                                  |
| ---------------------------- | ---- | ------------------------------------------------------------------------ |
| `EquityBelowFloor`           | 6358 | A risk-increasing action was blocked by the floor, or the breaker is set |
| `InvalidEquityFloorTransfer` | 6359 | A floor transfer broke one of the three transfer rules                   |
