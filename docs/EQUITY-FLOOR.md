# Equity floor

How the equity floor on delegated accounts works: what it enforces, what happens when it trips, and
how to move funds between subaccounts without tripping it.

This applies to accounts that Velocity creates and funds under its own authority, with an external
operator's trading key set as the `delegate` on each subaccount. The funds are deposited in USDT
across one or more subaccounts, and each subaccount carries an `equity_floor`: a minimum account
equity, denominated in USDT, that the subaccount must stay above. Velocity sets the floors when the
accounts are funded. The standard configuration sets the floors at 70% of the funded amount, which
is a protection target of at most 30% drawdown. On 1,000,000 USDT of funded capital, the floors
across the subaccounts sum to 700,000 USDT. The 30% figure is a target the mechanisms below
enforce rather than a synchronous onchain cap. See
[Timing and what the breaker does not guarantee](#timing-and-what-the-breaker-does-not-guarantee)
for the exact loss model.

Each subaccount also carries an `equity_floor_buffer`, the required headroom above the floor. The
floor itself is the breaker's trip threshold, and every risk-increasing action must clear the
higher line `floor + buffer`. The gap between the two lines is what makes the system safe to
operate against. No permitted action can take equity below `floor + buffer`, so the only way to
reach the floor is a passive drawdown that burns through the entire buffer first.

"Equity" here is the subaccount's **net equity**: unweighted asset value, plus funding-inclusive
unrealized PnL, minus unweighted spot liability value, all at live oracle prices (the program's
`calculate_user_equity`, mirrored by the SDK's `User.getNetUsdValue()`). This is what the account
is worth rather than the weighted margin numerator. Borrows subtract their full value, and
deposits and PnL count unweighted and unclamped. Every floor check and the breaker trip use this
one metric.
Spot balances and funding are valued as of their markets' last accrual: interest or funding
accrued since then is not applied inside the walk, the same staleness the margin engine carries,
bounded by the permissionless interest and funding cranks.
Floor and buffer are stored on the `User` account in `QUOTE_PRECISION` (1e6), so a 700,000 USDT
floor is `700_000_000_000`. A floor of `0` disables both checks. Only Velocity's admin can set or
change the floor and buffer. The delegate controls how the floor is split across subaccounts. See
[Moving funds between subaccounts](#moving-funds-between-subaccounts).

When an oracle a position depends on is invalid, because it is stale, too volatile, or too
uncertain, the checks stop trusting the equity value and fail closed in the direction of the
decision being made. Nothing bounds a value derived from an invalid price. The live price and its
own 5-minute TWAP can share the same stale value, so no stored price can bracket the true one.

Every gate that authorizes an action rejects with `InvalidOracle` while any oracle the
subaccount's positions depend on is invalid: withdrawals, risk-increasing placement and fills,
transfers out, trigger-order activation, and liquidator admission. Each rejects with
`EquityBelowFloor` instead when a fully valid value sits below `floor + buffer`. A bad price can
never authorize an action through the floor.

On the maker side the rejection is resolved before matching. A floored maker with any invalid
oracle has its risk-increasing orders pruned from the maker set, and its provably reducing orders
stay matchable. The maker is therefore unmatchable for new risk during the incident rather than
failing the taker's transaction. A rejected trigger likewise leaves the resting order in place, so
it activates normally once the feed recovers. Only a trusted value below the floor cancels it.

The force-cancel path fails closed the other way. Being below the raw floor counts as grounds only
when every oracle is valid and the trusted value sits below it, so a bad price can never make an
account look breached to a keeper. The standalone lifecycle instructions apply the same rule: the
reset, the cure transfers, and the floor-moving transfers all reject with `InvalidOracle` while
any relevant oracle is invalid, and resume when the feed recovers. A market in settlement is
valued at its expiry price, so its oracle is exempt from all of these validity requirements.

The breaker trip uses a user-favorable upper bound rather than trusting an invalid price.
Positions with valid oracles are valued at live prices exactly as everywhere else. For an invalid
oracle, a spot liability or short perp base leg counts as zero at any size, because this is its
sound maximum at a non-negative true price. Stored perp quote and funding legs still count
exactly. An asset or long base leg has no finite upper bound and keeps the trip blocked with
`InvalidOracle`, regardless of size. The quote oracle also stays strict because it converts every
perp pnl leg. No stored twap is used to bound an invalid oracle: the two sources can be stale or
wrong together, so a twap-derived allowance could understate a healthy account and falsely arm
the authority-wide breaker.

The lazy breaker trip decides with the same predicate as the permissionless trip. It must not
fail its host instruction, so it skips without an error wherever the trip would reject. During an
oracle outage a subaccount whose breach is unprovable, meaning it holds any asset or long with an
invalid oracle, stays untripped until the feed recovers. The next touch, or the permissionless
trip, then arms the breaker.

The exposure in that window is limited. The fail-closed gates reject everything risk-increasing
while any oracle is invalid, and DLOB match fills carry their own oracle-validity rule
(`FillOrderMatch`). An oracle the program cannot do margin with therefore blocks match execution
the same way the AMM's fill gates block AMM execution. Sibling subaccounts are not
authority-frozen during that window. That is accepted operational risk for the manually enrolled
and monitored set rather than an additional onchain state machine.

## What it enforces day to day

While a subaccount's equity is at or above `floor + buffer`, the checks have no effect. They only
bind on actions that could push equity below that line:

| Action                                                          | Check                                                             |
| --------------------------------------------------------------- | ----------------------------------------------------------------- |
| Risk-increasing order placement and fills (taker or maker side) | Rejected if equity is below floor + buffer                        |
| Withdrawals                                                     | Rejected if equity would end below floor + buffer                 |
| Transfers out of a subaccount (deposits, perp positions, pools) | Rejected if the debited side would end below floor + buffer       |
| Swaps                                                           | Rejected if equity would end below floor + buffer, except a strictly reducing swap (see below) |
| Reduce-only orders, closing positions, deposits, settles        | Always allowed, floor or no floor                                 |

A **strictly reducing swap** consumes an existing deposit to repay an existing borrow, no larger
than either: it creates no new liability and no new exposure, only deleverages. It stays allowed
below the gate and under the breaker, so an underwater subaccount can cure a borrow itself instead
of waiting for a liquidator and paying the liquidation discount. While the account is under floor
protection, the exempted swap's execution price is bounded: the output must be worth at least 99%
of the input at live oracle prices (`InvalidSwap` otherwise), so the exemption cannot be used to
route value out of the account through a bad venue.

All of these revert with `EquityBelowFloor` (error code 6358). The checks do not restrict
de-risking. They block only adding risk or withdrawing funds while equity is at or under the
required line. A subaccount sitting between the floor and `floor + buffer`, the buffer band, is
restricted to reduce-only activity but is not trippable. The breaker needs equity strictly below
the floor itself.

## The breaker

The checks above only apply to actions. Equity can also fall below the floor through trading
losses. The breaker covers that case.

The breaker arms in two ways, both against the same proof: a net-equity upper bound showing the
subaccount's equity is below its raw floor (not the buffered line: the buffer gates actions, the
floor arms the breaker). Positions with valid oracles are valued at live prices. Invalid-oracle
liabilities and shorts receive their sound zero upper bound rather than blocking the proof, and
any invalid-oracle asset or long blocks it, as described above. An authority-wide freeze can
therefore never be armed off a stale or degraded price.

1. **Lazily, on touch.** The actions that are allowed to run while a subaccount sits below its raw
   floor set the flag inline as a side effect of succeeding: a reducing perp fill, whether the
   subaccount is taker or maker, a strictly reducing swap, and a trigger order cancelled by the
   below-floor check. A below-floor subaccount with resting reduce-side orders is therefore frozen
   by the first counterparty fill against them, with no keeper involved. A delegate closing the
   losing position freezes the authority in that same transaction. The check costs nothing on
   subaccounts without a floor, and it is skipped once the breaker is set.
2. **By the permissionless `tripEquityFloorBreaker` instruction.** Any keeper can call it against a
   subaccount (it rejects with `InvalidOracle` when an invalid-oracle asset or long makes the
   breach unprovable, and with `SufficientCollateral` when the equity upper
   bound is not below the floor). Velocity runs a guard bot
   that watches every floored account and sends this trip. Under normal operation it lands within
   seconds of a breach, subject to RPC health and transaction inclusion. Because the instruction
   is permissionless, the guard bot is a backstop rather than a single point of failure. Any third
   party can trip a breached account.

The lazy path covers every actively traded account. The keeper path covers the remaining case of
a subaccount that breaches its floor and then sees no transactions at all.

Tripping sets the `equityBreakerTripped` flag on the authority's `UserStats` account. This flag is
authority-wide: it freezes every subaccount under the authority, not just the one that breached.
While it is set, all subaccounts reject:

- risk-increasing fills, both as taker and as maker. Resting risk-increasing trigger orders are
  cancelled instead of triggered, and the triggering keeper is paid no reward on that cancel.
- withdrawals,
- transfers out (deposit transfers, perp position transfers, pool transfers), except the cure
  transfer described below,
- swaps, except the price-bounded strictly reducing swap described above, which stays available so
  a frozen account can still repay a borrow out of its own deposits,
- acting as the liquidator in position-acquiring liquidations, and in swap-backed spot
  liquidations (`liquidate_spot_with_swap`), where the tokens flow through the authority's wallet
  but the liquidation fee is the same value capture the freeze exists to stop.

Reduce-only activity remains allowed: the delegate can still close positions, cancel orders,
deposit, and settle PnL. The accounts are not liquidated or seized.

A frozen authority can also cure the breach itself, from internal surplus. A **cure transfer** is a
funds-only `transferDepositByDelegate` (zero floor delta) into a subaccount whose equity is below
its buffered floor, and it stays allowed under the breaker. Eligibility is verified with the same
oracle-validity requirement the trip and the reset use (`InvalidOracle` otherwise), so it cannot be
decided off a stale or degraded price. The debited side must still clear its own
`floor + buffer` after the funds leave, so a cure can never create a new breach, and once the
credited side clears its buffered floor the exemption closes again. Partial cures compose: several
subaccounts can each contribute what they have to spare. The SDK plans this:
`manager.planCureTransfers()` returns the fund-only transfers that top every breached subaccount up
to just above its gate out of the others' spare equity (deepest breach first, donors drawn down no
further than just above their own gate), and `manager.cureBreaches()` submits them. Curing does not
clear the flag. It makes the reset safe to grant, because no subaccount is left below its floor
for a keeper to re-trip against.

The flag does not clear itself, even if equity recovers above the floor. Only Velocity's warm admin
can clear it, via `resetEquityFloorBreaker`, after a human has reviewed why it fired. If the
breaker trips, contact Velocity.

The reset is itself verified onchain: the instruction carries every live subaccount of the
authority (the count is pinned to `UserStats.number_of_sub_accounts`, so none can be omitted or
passed twice) together with their markets and oracles, and it reverts with
`InvalidEquityBreakerReset` (6368) unless every floored subaccount shows net equity at or above
its `floor + buffer` with all oracles valid at execution time. An approval that has gone stale,
because a subaccount drifted back into breach after it was reviewed, fails instead of unfreezing a
breached authority. The trip and the reset both prove their condition onchain. The reset's
validity requirement stays all-or-nothing and does not share the trip's upper bound on liabilities
and shorts. The reset proves equity above the line, the direction in which conceding value to an
unpriceable position would be unsound, so a dead oracle on any position blocks the reset until the
feed recovers. When
resumption is the business decision even though equity does not clear the floors, or a dead
oracle is blocking the reset, the admin lowers the floors first (`updateUserEquityFloor`),
explicitly and auditably, and then resets.

Because every permitted action leaves equity at or above `floor + buffer`, only losses that
consume the whole buffer can arm the breaker. The delegate cannot trade, withdraw, or transfer a
subaccount into a trippable state. The onchain checks reject the attempt instead.

## Timing and what the breaker does not guarantee

The per-subaccount gates are synchronous: an action that would breach the buffered floor reverts in
the transaction that attempts it. The authority-wide breaker is not: a subaccount can fall below
its raw floor through passive losses (funding, mark moves) without any transaction running, and a
program only executes inside a transaction, so the flag is set by the next qualifying transaction
to touch the chain, not at the moment of the breach. Between the breach and that transaction:

- The breached subaccount itself is already restricted to reduce-only activity by its own gate.
- Sibling subaccounts are not yet frozen. Each is still bounded by its own `floor + buffer` gate
  and its own margin requirements, so the additional exposure they can add in the window is capped
  by their own headroom, but it is not zero.
- The window closes at the first touch on the breached subaccount (a counterparty fill against its
  resting reduce-side orders, the delegate reducing, a keeper trigger) or at the guard bot's trip,
  whichever lands first. For an account with resting orders or any activity this is typically the
  next fill. For a fully idle account it is the guard bot's latency, and if the bot is down, it
  is whenever a third party trips it.

Two more things the breaker does not do:

- It does not close positions. A frozen account keeps its open positions, and passive losses can
  continue after the flag is set. Winding down is operational: Velocity's runbook uses the
  reduce-only `user close-positions` flow, which works while frozen.
- It does not restore equity. The floors bound losses via the gates, the trips, and the wind-down
  procedure together. Losses incurred in the window between breach and wind-down sit on the
  account like any trading loss.

The 70% floor configuration should therefore be read as: no delegate action can take a subaccount
below its buffered floor (synchronous), any breach freezes the authority at the next touch or trip
(asynchronous, typically fast), and the remaining exposure is passive market movement between the
breach and the completed wind-down.

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
await manager.cureBreaches(); // top breached subaccounts back up from the others' surplus
```

`rebalanceFloors` computes a proportional-to-equity floor split and applies it with zero-amount
transfers, so after PnL has moved equity around, every subaccount ends with the same relative
headroom; run it periodically or after large swings and floor placement stops being a concern.
`getStatus` reports a level per subaccount (`healthy`, `warning`, `critical`, `breached`) using
the same thresholds Velocity's guard bot monitors.

### The rules underneath

Every transfer is one `transferDepositByDelegate` instruction whose `equityFloorDelta` argument
shifts that much floor from the debited subaccount to the credited one, atomically with the funds.
A proportional share of the debited side's buffer travels with the floor:

```
buffer_delta = ceil(buffer_from * floor_delta / floor_from)
```

so moving 15% of a subaccount's floor also moves ~15% of its buffer, and shedding the whole floor
sheds the whole buffer. The sums of floors and of buffers across the subaccounts never change.
Only the split does. The rounding is up on the debited side, so no subaccount can end with floor
`0` while still holding buffer. A check-disabled subaccount holds neither.

The rounding also means many tiny floor moves shed buffer a little faster than proportional. A
long sequence of dust-sized moves can leave the debited side holding floor with little or no
buffer. That subaccount is still gated at its raw floor and can never be pushed below it by any
permitted action, but its working margin against passive drawdown shrinks. The guard bot's
headroom metrics make this visible, and the admin can restore the split with
`updateUserEquityFloor` at any time.

Three rules are enforced onchain, and violating any of them reverts the whole transfer with
`InvalidEquityFloorTransfer` (error code 6359):

1. The debited side must not already be below its floor. A subaccount that is already below its
   floor cannot move floor away to avoid a pending breaker trip. (This check uses the raw floor,
   not floor + buffer, so a subaccount inside the buffer band may still rebalance floor away.)
2. The debited side must end at or above its reduced floor plus its reduced buffer after the funds
   leave.
3. The credited side must end at or above its increased floor plus its increased buffer after the
   funds land, so the increased floor and buffer are backed by actual equity.

Because rules 2 and 3 evaluate against the post-transfer floor **and** buffer on both sides, no
sequence of permitted transfers can leave any subaccount below its own gate, let alone below its
trip line: a move that would do so reverts instead.

### The delta math

`transferQuote` computes the minimal floor that must move for the debited side to stay at or above
its buffered floor:

```
excess = max(0, net_equity_from - (floor_from + buffer_from))
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

The 120k of floor carries `ceil(20k * 120k / 350k) = 6,858` of buffer with it. After the transfer
the from side holds 250k of equity against a 230k floor and 13,142 of buffer, and the to side
gains 250k of equity, 120k of floor and 6,858 of buffer. The delta never exceeds `amount`, so the
credited side stays backed whenever it was backed before.

That minimum would land the debited side exactly on its buffered floor, and exact landings are
fragile: the SDK prices equity with the same live-oracle rules as the program, but at the boundary
the two can disagree by dust and revert the transfer. `transferQuote` therefore evaluates the
formula against `net_equity - $1` (a haircut), landing just above the line instead of on it.
(`transferDepositByDelegate` also accepts `'auto'` as the delta, which is this same formula without
the haircut, quote market only. It works away from the boundary, and `transferQuote` remains the
default.) The formula sizes the delta against the debited side's current buffer, but
the move itself sheds a share of that buffer too, so the debited side actually lands slightly above
its new gate rather than on it, a small conservatism on top of the haircut.

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

A concrete run-through, on 1,000,000 USDT funded across two subaccounts, with each call
paired to the math the program actually executes. Floors sum to 700,000 (the 70% configuration) and
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

Step 2: a transfer past the slack drags floor along, one for one, and the floor drags its share of
buffer. Another 100,000 out of subaccount 0, same call. Only 40,000 of slack remains, so 60,000 of
floor must travel, carrying `ceil(10,000 * 60,000 / 400,000) = 1,500` of buffer:

```
excess = max(0, 450,000 - 410,000) = 40,000
delta  = min(max(100,000 - 40,000, 0), 400,000) = 60,000
buffer_delta = ceil(10,000 * 60,000 / 400,000) = 1,500

          equity      floor      buffer     gate       slack
sub 0     350,000     340,000     8,500     348,500      1,500
sub 1     650,000     360,000    11,500     371,500    278,500   // check: 650,000 >= 371,500, passes
```

Sub 0 lands a little above its gate. The delta was sized against the old 10,000 buffer, but the
move shed 1,500 of it, so 1,500 of slack remains. The tables show the unpadded minimum so the
arithmetic stays round. In practice `transferQuote`'s haircut moves a dollar more floor and parks
sub 0 higher still.

Step 3: what a rejection looks like. Withdrawing 50,000 from subaccount 0 (a withdrawal goes to
the outside, so no floor can travel with it):

```ts
await velocityClient.withdraw(new BN(50_000).mul(QUOTE_PRECISION), 0, tokenAccount);
// reverts: EquityBelowFloor (6358)
```

```
equity after = 350,000 - 50,000 = 300,000
gate         = 348,500
300,000 < 348,500  ->  revert; sub 0's slack is 1,500, so at most dust may leave
```

Step 4: floor can move without funds. Equity now sits mostly on subaccount 1, so the delegate
shifts 200,000 of floor onto it with a zero-amount transfer. The floor carries
`ceil(8,500 * 200,000 / 340,000) = 5,000` of buffer. All three rules evaluated:

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
rule 1  sub 0 not below its raw floor:   350,000 >= 340,000                          ok
rule 2  sub 0 ends at/above new gate:    350,000 >= (340,000 - 200,000) + 3,500      ok
rule 3  sub 1 ends at/above new gate:    650,000 >= (360,000 + 200,000) + 16,500     ok

          equity      floor      buffer     gate       slack
sub 0     350,000     140,000     3,500     143,500    206,500
sub 1     650,000     560,000    16,500     576,500     73,500   // floors sum 700,000, buffers 20,000
```

Step 5: and what floor placement cannot do. Pushing another 90,000 of floor onto subaccount 1
(carrying `ceil(3,500 * 90,000 / 140,000) = 2,250` of buffer) would leave its gate at 668,750
against 650,000 of equity, so rule 3 rejects it with `InvalidEquityFloorTransfer`: floor only sits
where equity backs it. Note what was conserved through every step: total floor (700,000 always),
total buffer (20,000 always) and total slack (280,000 after step 2, reshuffled since).
Transfers relocate headroom. Only PnL and deposits change its total.

Step 6: the manager does step 4's thinking automatically. `rebalanceFloors` targets a
proportional-to-equity split:

```ts
await manager.rebalanceFloors();
```

```
total equity = 350,000 + 650,000 = 1,000,000
target 0     = 700,000 * 350,000 / 1,000,000 = 245,000
target 1     = 700,000 * 650,000 / 1,000,000 = 455,000
move         = 105,000 of floor from sub 1 back to sub 0 (zero-amount transfer),
               carrying ceil(16,500 * 105,000 / 560,000) = 3,094 of buffer

          equity      floor      buffer     gate       equity / floor
sub 0     350,000     245,000     6,594     251,594    1.43
sub 1     650,000     455,000    13,406     468,406    1.43     equal relative headroom
```

The manual move in step 4 over-rotated: sub 1 ended with 73,500 of slack against sub 0's
206,500. The rebalancer evens the ratio out, so both subaccounts sit equally far from their floors
in relative terms. At no point in this session was any subaccount trippable. Every state above
keeps equity at or above the gate, a full buffer above the trip line.

## The invariant

All of the checks above enforce one rule:

> Every subaccount's equity must cover its floor plus its buffer, and the floors always sum to the
> agreed total.

The program pins the sums (only the transfer instruction can move floor and buffer between
subaccounts, and it conserves both. Only Velocity's admin can change the totals. The program
checks net equity against the buffered floor per subaccount on every risk-increasing action. The
aggregate consequence is that no permitted action can leave total equity across the subaccounts
below the agreed total floor, for example 70% of the funded amount, with the buffers as working
margin on top. The breaker handles passive losses past a floor, on the timing described above.

As long as the rule holds, the delegate can split the funds across subaccounts and move funds and
floor between them freely. If losses push any subaccount below its floor, the breaker trips and all
subaccounts become reduce-only until a Velocity admin resets the flag.

## Monitoring

The SDK mirrors the onchain checks. All of them measure net equity (`user.getNetUsdValue()`), the
same metric the program uses:

| Helper                                    | What it reports                                                                |
| ----------------------------------------- | ------------------------------------------------------------------------------ |
| `user.getNetUsdValue()`                   | Net equity: the value every floor check compares against                       |
| `user.isBelowEquityFloor()`               | `true` when net equity is below the floor (point value)                        |
| `user.provesEquityFloorBreach(slot)`      | `true` when the onchain trip would fire (upper-bound verdict, `getTripNetEquity`) |
| `user.isBelowBufferedEquityFloor()`       | `true` when net equity is below floor + buffer (actions rejecting)             |
| `user.getBufferedEquityFloor()`           | `floor + buffer`: the line risk-increasing actions must clear                  |
| `user.getEquityAboveFloor()`              | Headroom above the trip threshold. `null` when no floor is set                 |
| `user.getEquityAboveBufferedFloor()`      | Headroom above the action gate. `null` when no floor is set                    |
| `getEquityFloorLevel(equity, floor, buf)` | `healthy` / `warning` / `critical` / `breached`                                |
| `userStatsAccount.equityBreakerTripped`   | Whether the authority-wide breaker is currently set                            |

Alerts should fire while a subaccount is still `warning`, meaning inside two buffers of the floor.
`critical` means risk-increasing actions are already rejecting, and `breached` means the breaker
can fire at any moment. The breaker is permissionless, so Velocity's guard bot is not the only
party that can call it. The guard bot attempts the trip on either signal, the point-value
`breached` level or `provesEquityFloorBreach`. The point value prices every position at the live
oracle with no validity check, so an invalid oracle printing high can hide a breach the
upper-bound verdict still proves. A third-party keeper should do the same.

## Using from Rust

`velocity-rs` carries the same surface for Rust consumers. The generated bindings already include
`equity_floor_buffer` on the `User` account and the `equity_floor_delta` argument on
`transfer_deposit_by_delegate`, and the onchain predicates come straight from the program crate
the SDK re-exports. The client-side math mirrors the TypeScript helpers exactly (same thresholds,
same tests):

```rust
use velocity_rs::math::equity_floor::{
	calculate_equity_floor_auto_delta, equity_floor_level, EquityFloorLevel,
	DEFAULT_WARNING_BUFFER_MULTIPLE,
};

// size the floor delta a transfer must carry, as transferQuote does.
// Pad it when the debit side would land near its buffered floor.
let delta = calculate_equity_floor_auto_delta(
	amount,
	net_equity, // i128, from the program crate's calculate_user_equity
	user.equity_floor,
	user.equity_floor_buffer,
);

// classify for monitoring. Levels order by severity, so worst-of is max()
let level = equity_floor_level(
	net_equity,
	user.equity_floor,
	user.equity_floor_buffer,
	DEFAULT_WARNING_BUFFER_MULTIPLE,
);
if level >= EquityFloorLevel::Critical {
	// risk-increasing actions are rejecting on this subaccount
}

// the program's own predicates, for exact onchain semantics
let gated = user.is_below_buffered_equity_floor(net_equity);
let trippable = user.is_below_equity_floor(net_equity);
```

There is no Rust equivalent of the `EquityFloorManager`. A Rust client passes the computed delta
to `transfer_deposit_by_delegate` directly and applies its own haircut when landing near the
line.

## How the pieces fit together

| Layer          | Where                                      | Role                                                         |
| -------------- | ------------------------------------------ | ------------------------------------------------------------ |
| Program        | `programs/velocity`                        | The only enforcement: gates, trip, freeze                    |
| TypeScript SDK | `packages/sdk`                             | Mirrors the checks; `EquityFloorManager`                     |
| Rust SDK       | `rust/velocity-rs`                         | Mirrors the math; bindings and predicates                    |
| Guard bot      | `apps/keeper-bots-v2` (`equityFloorGuard`) | Watches, alerts, and trips the breaker                       |
| Admin CLI      | `packages/cli-admin` (`user` commands)     | Velocity's lifecycle tooling: set, inspect, wind down, reset |

The program is the only layer that enforces anything. Floor and buffer live on each `User`
account and every risk-increasing instruction checks equity against `floor + buffer` on the
subaccount it is already operating on. The authority-wide part is a single flag on `UserStats`,
set lazily by the exempt paths themselves or by the permissionless trip, and cleared only by the
warm admin. The SDKs mirror those checks rather than adding rules of their own, and every consumer
that reports a level uses the same classifier: the manager, the guard bot, and the CLI.

Lifecycle:

1. Velocity funds the subaccounts and sets floors and buffers (`user set-equity-floor`).
2. The delegate trades and rebalances freely; every action is gated at `floor + buffer`.
3. The guard bot watches headroom and alerts on `warning` and `critical`.
4. If losses cross a floor, the breaker trips (at the first touch on the breached subaccount, or
   at the guard bot's trip, whichever lands first) and every subaccount goes reduce-only.
5. The delegate cures the breach while frozen: deposits, or cure transfers from sibling
   subaccounts' surplus (`manager.cureBreaches()`).
6. Velocity inspects (`user equity-floor-status`), winds down if needed (`user close-positions`,
   reduce-only, so it works while frozen), and clears the flag after review
   (`user reset-equity-breaker`). The reset proves onchain that every subaccount clears its
   `floor + buffer` and reverts otherwise, so a stale approval cannot unfreeze a breached
   authority.

## Quick reference

| Instruction                 | Who can call        | What it does                                                           |
| --------------------------- | ------------------- | ---------------------------------------------------------------------- |
| `updateUserEquityFloor`     | Velocity admin      | Sets a subaccount's floor and buffer (changes the totals)              |
| `transferDepositByDelegate` | The delegate        | Moves funds and floor between subaccounts, conserving the floor sum    |
| `tripEquityFloorBreaker`    | Anyone              | Proves one subaccount is below its floor, freezes the whole authority  |
| `resetEquityFloorBreaker`   | Velocity warm admin | Proves every subaccount clears floor + buffer, then clears the breaker |

| Error                        | Code | Meaning                                                                       |
| ---------------------------- | ---- | ----------------------------------------------------------------------------- |
| `EquityBelowFloor`           | 6358 | A risk-increasing action was blocked at floor + buffer, or the breaker is set |
| `InvalidEquityFloorTransfer` | 6359 | A floor transfer broke one of the three transfer rules                        |
| `InvalidSwap`                | 6248 | Among other swap failures: a strictly reducing swap under floor protection breached the 1% oracle value bound |
| `InvalidEquityBreakerReset`  | 6368 | A breaker reset was refused: a subaccount is below its floor + buffer, or the subaccount set was incomplete |
