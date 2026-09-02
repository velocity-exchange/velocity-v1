---
'@velocity-exchange/sdk': minor
---

Limit orders no longer accept an oracle price offset.

The program refuses any `OrderType.LIMIT` order whose `oraclePriceOffset` is nonzero with
`InvalidOrderOracleOffset` (6055). An oracle-floating limit cannot rest on a CLOB, so such an
order could only strand in `User.orders` on the legacy DLOB. Use a PropAMM quoter for an
oracle-relative maker quote, or a repriced fixed-price limit. `OrderType.ORACLE` market orders
keep their oracle-relative auctions; the `Order.oraclePriceOffset` field and the order params
shape are unchanged.

Three CLOB-era instructions are renamed before first release: `fillPerpOrderV1` is now
`fillLegacyDlobOrder` (it fills only orders the legacy endpoints created, and is deleted with
them), `triggerOrderV1` is now `triggerMarketOrderV1`, and `triggerClobOrder` is now
`triggerLimitOrderV1` (the pair partitions trigger orders by type: a fired limit rests whole, a
fired market fills first). Client methods and instruction builders follow the new names.
