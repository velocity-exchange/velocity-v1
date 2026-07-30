---
'@velocity-exchange/sdk': minor
---

Cross-match crank: new permissionless `crank_cross_match` instruction (fills two crossed resting sources against each other with the protocol `User` as the pass-through taker; reverts unless the spread nets positive after both legs' taker fees) plus its relay discovery: `resolve_clob_crank_cross`, five-condition `ClobCrankConditionsV0` (3528 bytes), a trailing `makersIncludeStats` arg on `crank_cross_match`, and errors `CrossMatchImbalanced` / `CrossMatchUnprofitable` in the IDL.
