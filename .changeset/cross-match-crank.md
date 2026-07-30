---
'@velocity-exchange/sdk': minor
---

Cross-match crank: new permissionless `crank_cross_match` instruction (fills two crossed resting sources against each other with the protocol `User` as the pass-through taker; reverts unless the spread nets positive after both legs' taker fees) plus its relay discovery: `resolve_clob_crank_cross`, five-condition `ClobCrankConditionsV0` (3528 bytes), and errors `CrossMatchImbalanced` / `CrossMatchUnprofitable` in the IDL. CLOB user identity moved to derivable form (`(authority, subAccountId)` on order nodes and the quoter CPI wire), so relay resolvers can stage full `(User, UserStats)` pairs from a book node alone.
