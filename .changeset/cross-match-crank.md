---
'@velocity-exchange/sdk': minor
---

Cross-match crank: new permissionless `crank_cross_match` instruction (fills two crossed resting sources against each other with the protocol `User` as the pass-through taker; reverts unless the spread nets positive after both legs' taker fees) and errors `CrossMatchImbalanced` / `CrossMatchUnprofitable` in the IDL.
