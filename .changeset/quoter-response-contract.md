---
'@velocity-exchange/sdk': minor
---

Velocity now validates an external quoter's CPI response instead of trusting it, and the
`users` CPI argument becomes a fixed-width set. A quoter's `execute_v0` response may only move
balances for users that quoter may act against (a `Custom` entry's registry `user`; a `Clob`
entry's makers actually resting on its own book; never the taker), the executed volume and
price are bound to the levels that entry quoted in the same transaction, and a quote response
with a zero price/size or out-of-order levels is rejected on ingestion. Both CPI legs now take
the perp market index and reject an entry registered for a different market.
`QuoteArgsV0::users` / `ExecuteArgsV0::users` change from `Option<Vec<ClobUserRefV0>>` to
`QuoterUserSetV0` (a `u8` count plus `[ClobUserRefV0; 48]`, empty = unrestricted), which is
breaking for third-party quoter programs. New error codes `InvalidQuoterResponse`,
`QuoterOverfilled`, `QuoterFillOffQuote`, `QuoterSubjectNotPermitted` and
`TooManyQuoterWireUsers`. The SDK gains the TS mirror of the new bounds —
`areQuotedLevelsValid`, `quotedPrefix`, `isExecutedNotionalInQuote`,
`isChangeNotionalInQuote` and `RouterQuotedPrefix` in `math/router` — so a client can predict
whether a router fill lands.
