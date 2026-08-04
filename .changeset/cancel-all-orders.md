---
'@velocity-exchange/sdk': minor
---

Cancel-all for makers on both quoter types. New velocity instruction
`cancel_all_clob_orders` (`CancelAllClobOrdersParams { marketIndex, sides }`, where
`sides` is the new `ClobCancelSides` enum — `Bids` / `Asks` / `Both`) pulls every
order a `User` holds on a side, or both, in one CPI into the CLOB's new
`cancel_all_v0`. The open-order aggregates unwind from per-side base totals plus
one order count, so sweeping twenty orders costs the same bookkeeping as
sweeping one — measured end to end, an 8-order ladder is 14.4k CU against 90.7k
for eight `cancel_clob_order` instructions.

The book caps a single sweep at 128 removals and reports whether it finished;
velocity unwinds what was actually removed and the call is safe to repeat. Not
gated on the quoter entry's active/approved flags or on the exchange pause, so a
maker can always pull quotes off a killed, de-listed or halted book. The CLOB
emits a new `OrdersCancelRecordV0` naming every removed order id, so an indexer
reconciles the book from one record instead of N cancel records.

The midpoint quoter gains its own `cancel_all_v0` (`{ sides, clearMid }`), which
zeros the named sides' live rungs and optionally the mid, and accepts either the
hot or the config key.

IDL only for the SDK — no client method, matching the rest of the CLOB maker
surface (`place_clob_order` / `cancel_clob_order` have none either). No account
layout changed.
