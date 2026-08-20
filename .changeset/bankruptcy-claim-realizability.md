---
'@velocity-exchange/sdk': patch
---

Stop `isUserBankrupt` vetoing on a positive perp claim or its market's PnL pool, mirroring the
program: the resolvers now recover what the pool can pay and forfeit the rest, so no pool state
blocks admission. `getResolveSpotBankruptcyIx` also passes the quote spot market writable, which that
instruction now requires.
