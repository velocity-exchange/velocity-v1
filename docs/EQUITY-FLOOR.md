# Equity floor

How the equity floor on delegated accounts works: what it enforces, what happens when it trips, and
how to move funds between subaccounts without tripping it.

This applies to accounts that Velocity creates and funds under its own authority, with the borrowing
maker's trading key set as the `delegate` on each subaccount. The loan is deposited in USDT across
one or more subaccounts, and each subaccount carries an `equity_floor`: a minimum account equity,
denominated in USDT, that the subaccount must stay above. Velocity sets the floors when the accounts
are funded; the standard arrangement is 70% of the loan amount, i.e. a maximum loss of 30% of the
loan. On a 1,000,000 USDT loan, the floors across the subaccounts sum to 700,000 USDT.

Each subaccount also carries an `equity_floor_buffer`: required headroom above the floor. The floor
itself is the trip threshold of the breaker; every risk-increasing action must clear the higher line
`floor + buffer`. The gap between the two lines is what makes the system safe to operate against:
no permitted action can take equity below `floor + buffer`, so the only way to reach the floor is a
passive drawdown that burns through the entire buffer first.

"Equity" here is the subaccount's cross-margin total collateral: deposits plus unrealized PnL,
valued at oracle prices (the withdraw and transfer paths use strict, TWAP-bounded oracle pricing).
Floor and buffer are stored on the `User` account in `QUOTE_PRECISION` (1e6), so a 700,000 USDT
floor is `700_000_000_000`. A floor of `0` disables both checks. Only Velocity's admin can set or
change the floor and buffer; what the delegate controls is how the floor is split across subaccounts
(see [Moving funds between subaccounts](#moving-funds-between-subaccounts)).

## What it enforces day to day

While a subaccount's equity is at or above `floor + buffer`, the checks have no effect. They only
bind on actions that could push equity below that line:

| Action                                                          | Check                                                             |
| --------------------------------------------------------------- | ----------------------------------------------------------------- |
| Risk-increasing order placement and fills (taker or maker side) | Rejected if equity is below floor + buffer                        |
| Withdrawals                                                     | Rejected if equity would end below floor + buffer                 |
| Transfers out of a subaccount (deposits, perp positions, pools) | Rejected if the debited side would end below floor + buffer       |
| Swaps                                                           | Rejected if equity would end below floor + buffer                 |
| Reduce-only orders, closing positions, deposits, settles        | Always allowed, floor or no floor                                 |

All of these revert with `EquityBelowFloor` (error code 6358). The checks do not restrict
de-risking; they only block adding risk or withdrawing funds while equity is at or under the
required line. A subaccount sitting between the floor and `floor + buffer` (the buffer band) is
restricted to reduce-only activity but is not trippable: the breaker needs equity strictly below
the floor itself.

## The breaker

The checks above only apply to actions. Equity can also fall below the floor through trading
losses; the breaker covers that case.

`tripEquityFloorBreaker` is a permissionless instruction: any keeper can call it against a
subaccount, and the on-chain proof is simply a margin calculation showing that subaccount's equity
is below its floor (not the buffered line: the buffer gates actions, the floor arms the breaker).
Velocity runs a guard bot that watches every floored account, so once any subaccount drops below
its floor, expect the breaker to be tripped within seconds.

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
can clear it, via `resetEquityFloorBreaker`, after a human has reviewed why it fired. If the
breaker trips, contact Velocity.

Because every permitted action leaves equity at or above `floor + buffer`, the breaker can only be
armed by losses eating through the buffer. The delegate cannot trade, withdraw, or transfer a
subaccount into a trippable state; the on-chain checks reject the attempt instead.

## Moving funds between subaccounts

The floor is checked per subaccount, and each subaccount's floor is its own field. Moving USDT from
subaccount A to subaccount B without also moving floor leaves A clearing its full original floor
with less collateral in it, so a transfer must carry floor along with the funds. The SDK's
`EquityFloorManager` handles all of that; this is the way to move USDT between subaccounts:

```ts
const manager = new EquityFloorManager(velocityClient);

// funds plus exactly as much floor as the debited side must shed
// (plus a small safety pad), in one instruction
await manager.transferQuote(amount, fromSubAccountId, toSubAccountId);
```

The same class covers the rest of day-to-day management:

```ts
manager.getStatus(); // aggregate + per-subaccount equity, floor, buffer, headroom, level
manager.getMaxWithdrawable(subAccountId); // most that can leave to the outside
manager.getMaxQuoteTransferable(from, to); // most that can move between subaccounts
await manager.rebalanceFloors(); // re-split the floor to match where the equity sits
```

`rebalanceFloors` computes a proportional-to-equity floor split and applies it with zero-amount
transfers, so after PnL has moved equity around, every subaccount ends with the same relative
headroom; run it periodically or after large swings and floor placement stops being a concern.
`getStatus` reports a level per subaccount (`healthy`, `warning`, `critical`, `breached`) using
the same thresholds Velocity's guard bot monitors.

### The rules underneath

Every transfer is one `transferDepositByDelegate` instruction whose `equityFloorDelta` argument
shifts that much floor from the debited subaccount to the credited one, atomically with the funds.
The sum of floors across the subaccounts never changes; only the split does. Three rules are
enforced on-chain, and violating any of them reverts the whole transfer with
`InvalidEquityFloorTransfer` (error code 6359):

1. The debited side must not already be below its floor. A subaccount that is already below its
   floor cannot move floor away to avoid a pending breaker trip. (This check uses the raw floor,
   not floor + buffer, so a subaccount inside the buffer band may still rebalance floor away.)
2. The debited side must end at or above its reduced floor plus its buffer after the funds leave.
3. The credited side must end at or above its increased floor plus its own buffer after the funds
   land, so the increased floor is backed by actual equity.

### The delta math

`transferQuote` computes the minimal floor that must move for the debited side to stay at or above
its buffered floor:

```
excess = max(0, collateral_from - (floor_from + buffer_from))
delta  = min(max(amount - excess, 0), floor_from)
```

The first `excess` USDT of the transfer is equity held above the buffered floor and carries no
floor with it; anything beyond that must take floor along, capped at the floor the debited side
has. Worked example, from side holding 500k USDT of equity against a 350k floor and a 20k buffer,
transferring 250k:

```
excess = 500k - (350k + 20k)         = 130k
delta  = min(250k - 130k, 350k)      = 120k
```

After the transfer the from side holds 250k of equity against a 230k floor and its 20k buffer, and
the to side gains 250k of equity and 120k of floor. The delta never exceeds `amount`, so the
credited side stays backed whenever it was backed before.

That minimum would land the debited side exactly on its buffered floor, and exact landings are
fragile: the SDK prices collateral with the same strict oracle rules as the program, but at the
boundary the two can disagree by dust and revert the transfer. `transferQuote` therefore evaluates
the formula against `collateral - $1` (a haircut), landing just above the line instead of on it.
(`transferDepositByDelegate` also accepts `'auto'` as the delta, which is this same formula without
the haircut, quote market only; it works away from the boundary, but `transferQuote` is the
default for a reason.)

### Custom floor placement

For the cases where the delegate is choosing floor placement rather than asking for the minimum,
call the instruction directly with an explicit `equityFloorDelta`:

```ts
// move 250k USDT from subaccount 0 to subaccount 1, carrying 120k of floor with it
await velocityClient.transferDepositByDelegate(
	new BN(250_000).mul(QUOTE_PRECISION), // amount, token precision (USDT = 1e6)
	0, // marketIndex: USDT
	0, // fromSubAccountId
	1, // toSubAccountId
	new BN(120_000).mul(QUOTE_PRECISION) // equityFloorDelta
);
```

Reach for this when:

- rebalancing floor without moving funds: a zero `amount` with a delta moves floor alone (this is
  what `rebalanceFloors` submits under the hood);
- deliberately shedding more floor than the minimum, to leave the debited side extra slack;
- transferring on a non-quote spot market, where the SDK cannot value the tokens and the delta
  must be computed by the caller;
- a planned flow needs the delta without sending: `manager.planQuoteTransfer(amount, from, to)`
  returns the padded parameters for inspection.

The same three rules apply to every variant; there is no way to place floor where equity does not
back it.

## A worked session

A concrete run-through, on a 1,000,000 USDT loan split across two subaccounts, with each call
paired to the math the program actually executes. Floors sum to 700,000 (the 70% arrangement) and
each subaccount carries a 10,000 buffer. "Gate" below means `floor + buffer`, the line
risk-increasing actions must clear.

```
          equity      floor      buffer     gate       slack (equity - gate)
sub 0     600,000     400,000    10,000     410,000    190,000
sub 1     400,000     300,000    10,000     310,000     90,000
```

Step 1: a transfer that fits inside slack moves no floor. The delegate moves 150,000 from
subaccount 0 to subaccount 1:

```ts
await manager.transferQuote(new BN(150_000).mul(QUOTE_PRECISION), 0, 1);
```

```
excess = max(0, 600,000 - 410,000) = 190,000   // sub 0's slack
delta  = min(max(150,000 - 190,000, 0), 400,000) = 0   // amount fits, no floor moves

          equity      floor      gate       slack
sub 0     450,000     400,000    410,000     40,000
sub 1     550,000     300,000    310,000    240,000
```

Step 2: a transfer past the slack drags floor along, one for one. Another 100,000 out of
subaccount 0, same call. Only 40,000 of slack remains, so 60,000 of floor must travel:

```
excess = max(0, 450,000 - 410,000) = 40,000
delta  = min(max(100,000 - 40,000, 0), 400,000) = 60,000

          equity      floor      gate       slack
sub 0     350,000     340,000    350,000          0   // exactly at its gate
sub 1     650,000     360,000    370,000    280,000   // check: 650,000 >= 370,000, passes
```

Sub 0 lands on its gate: legal (the check is a strict less-than) but with zero slack, so nothing
more may leave it. The tables show the unpadded minimum so the arithmetic stays round; in practice
`transferQuote`'s haircut moves a dollar more floor and parks sub 0 just above the gate instead of
exactly on it.

Step 3: what a rejection looks like. Withdrawing 50,000 from subaccount 0 (a withdrawal goes to
the outside, so no floor can travel with it):

```ts
await velocityClient.withdraw(new BN(50_000).mul(QUOTE_PRECISION), 0, tokenAccount);
// reverts: EquityBelowFloor (6358)
```

```
equity after = 350,000 - 50,000 = 300,000
gate         = 350,000
300,000 < 350,000  ->  revert; sub 0's slack is 0, so nothing may leave
```

Step 4: floor can move without funds. Equity now sits mostly on subaccount 1, so the delegate
shifts 200,000 of floor onto it with a zero-amount transfer. All three rules evaluated:

```ts
await velocityClient.transferDepositByDelegate(
	ZERO,
	0,
	0, // from: sheds floor
	1, // to: takes floor
	new BN(200_000).mul(QUOTE_PRECISION)
);
```

```
rule 1  sub 0 not below its raw floor:   350,000 >= 340,000                      ok
rule 2  sub 0 ends at/above new gate:    350,000 >= (340,000 - 200,000) + 10,000  ok
rule 3  sub 1 ends at/above new gate:    650,000 >= (360,000 + 200,000) + 10,000  ok

          equity      floor      gate       slack
sub 0     350,000     140,000    150,000    200,000
sub 1     650,000     560,000    570,000     80,000     floors still sum to 700,000
```

Step 5: and what floor placement cannot do. Pushing another 90,000 of floor onto subaccount 1
would leave its gate at 660,000 against 650,000 of equity, so rule 3 rejects it with
`InvalidEquityFloorTransfer`: floor only sits where equity backs it. Note what was conserved
through every step: total floor (700,000 always) and total slack (280,000 after step 2, just
reshuffled since). Transfers relocate headroom; only PnL and deposits change its total.

Step 6: the manager does step 4's thinking automatically. `rebalanceFloors` targets a
proportional-to-equity split:

```ts
await manager.rebalanceFloors();
```

```
total equity = 350,000 + 650,000 = 1,000,000
target 0     = 700,000 * 350,000 / 1,000,000 = 245,000
target 1     = 700,000 * 650,000 / 1,000,000 = 455,000
move         = 105,000 of floor from sub 1 back to sub 0 (zero-amount transfer)

          equity      floor      gate       equity / floor
sub 0     350,000     245,000    255,000    1.43
sub 1     650,000     455,000    465,000    1.43     equal relative headroom
```

The manual move in step 4 over-rotated (sub 1 ended with 80,000 of slack against sub 0's
200,000); the rebalancer evens the ratio out so both subaccounts are equally far from their
floors in relative terms. At no point in this whole session was any subaccount trippable: every
state above keeps equity at or above the gate, a full buffer above the trip line.

## The invariant

All of the checks above enforce one rule:

> Every subaccount's equity must cover its floor plus its buffer, and the floors always sum to the
> agreed total.

The program pins the sum (only the transfer instruction can move floor, and it conserves it; only
Velocity's admin can change the total or the buffers), and checks equity against the buffered floor
per subaccount on every risk-increasing action. The aggregate consequence is that total equity
across the subaccounts always covers the agreed total floor, e.g. 70% of the loan, with the buffers
as working margin on top.

As long as the rule holds, the delegate can split the loan across subaccounts and move funds and
floor between them freely. If losses push any subaccount below its floor, the breaker trips and all
subaccounts become reduce-only until a Velocity admin resets the flag.

## Monitoring

The SDK mirrors the on-chain checks:

| Helper                                    | What it reports                                                                |
| ----------------------------------------- | ------------------------------------------------------------------------------ |
| `user.isBelowEquityFloor(strict)`         | `true` when equity is below the floor (trip condition)                         |
| `user.isBelowBufferedEquityFloor(strict)` | `true` when equity is below floor + buffer (actions rejecting)                 |
| `user.getBufferedEquityFloor()`           | `floor + buffer`: the line risk-increasing actions must clear                  |
| `user.getEquityAboveFloor(strict)`        | Headroom above the trip threshold; `null` when no floor is set                 |
| `user.getEquityAboveBufferedFloor(strict)`| Headroom above the action gate; `null` when no floor is set                    |
| `getEquityFloorLevel(equity, floor, buf)` | `healthy` / `warning` / `critical` / `breached`                                |
| `userStatsAccount.equityBreakerTripped`   | Whether the authority-wide breaker is currently set                            |

Pass `strict = true` to match the TWAP-bounded pricing the withdraw/transfer paths use. Alerts
should fire while a subaccount is still `warning` (inside two buffers of the floor); `critical`
means risk-increasing actions are already rejecting, and `breached` means the breaker can fire at
any moment. The breaker is permissionless, so Velocity's guard bot is not the only party that can
call it.

## How the pieces fit together

End-to-end, the system is four layers, each thin on its own:

| Layer      | Where                                        | Role                                                        |
| ---------- | -------------------------------------------- | ----------------------------------------------------------- |
| Program    | `programs/velocity`                          | The only enforcement: gates, trip, freeze                   |
| SDK        | `packages/sdk`                               | Faithful mirror of the checks, plus the manager             |
| Guard bot  | `apps/keeper-bots-v2` (`equityFloorGuard`)   | Watches, alerts, and trips the breaker                      |
| Admin CLI  | `packages/cli-admin` (`user` commands)       | Velocity's lifecycle tooling: set, inspect, wind down, reset |

The program holds two `u64` fields on each `User` account, `equity_floor` and
`equity_floor_buffer` (carved from existing padding, so the account layout and size never
changed), and two predicates over them: below-floor (the trip condition) and below-buffered-floor
(the action gate). Every risk-increasing path evaluates the gate against the margin engine's
total collateral, which it has already computed for the margin check itself: order placement,
taker and maker fills, withdrawals, swaps, and all transfers out. The checks read only the one
`User` account each path already has loaded, which is why the floor is per subaccount rather than
authority-wide: an aggregate check would need every sibling subaccount's collateral in every hot
path. The authority-wide part is a single byte on `UserStats`, set by the permissionless
`tripEquityFloorBreaker` (whose proof is just a margin calculation) and cleared only by the warm
admin; while set, the same gates reject on every subaccount regardless of individual health.

The SDK mirrors the program rather than adding rules of its own. `UserAccount` carries the two
fields, the `User` class reimplements the two predicates over the same strict oracle pricing, and
one small pure-math module (`math/margin`) holds the delta formula and the level classifier. The
`EquityFloorManager` composes those primitives over `transferDepositByDelegate`; it introduces no
new authority and nothing it does could not be done with raw instruction calls. Everything that
displays or decides, the manager, the guard bot, and the admin CLI's status command, imports the
same classifier, so `critical` means the same thing in a maker's dashboard, Velocity's metrics,
and an operator's terminal.

The guard bot polls every floored subaccount with the same strict pricing the program uses,
publishes headroom and level metrics, alerts on level transitions and sharp headroom drops, and
submits the trip the moment a subaccount's equity is provably below its floor. Because the trip is
permissionless and its proof is on-chain, the bot holds no privileged key; it is an alarm clock,
not an authority.

A full lifecycle reads like this: Velocity funds the subaccounts and sets floors and buffers
(`user set-equity-floor`); the delegate trades and rebalances freely while every action is gated
at `floor + buffer`; the guard bot watches headroom the whole time; if losses burn through a
buffer, the breaker trips and every subaccount goes reduce-only; Velocity inspects
(`user equity-floor-status`), winds down positions if needed (`user close-positions`, itself
purely reduce-only, which is why it works while frozen), and after review clears the flag
(`user reset-equity-breaker`).

## Quick reference

| Instruction                 | Who can call        | What it does                                                           |
| --------------------------- | ------------------- | ---------------------------------------------------------------------- |
| `updateUserEquityFloor`     | Velocity admin      | Sets a subaccount's floor and buffer (changes the totals)              |
| `transferDepositByDelegate` | The delegate        | Moves funds and floor between subaccounts, conserving the floor sum    |
| `tripEquityFloorBreaker`    | Anyone              | Proves one subaccount is below its floor, freezes the whole authority  |
| `resetEquityFloorBreaker`   | Velocity warm admin | Clears the breaker after review                                        |

| Error                        | Code | Meaning                                                                       |
| ---------------------------- | ---- | ----------------------------------------------------------------------------- |
| `EquityBelowFloor`           | 6358 | A risk-increasing action was blocked at floor + buffer, or the breaker is set |
| `InvalidEquityFloorTransfer` | 6359 | A floor transfer broke one of the three transfer rules                        |
