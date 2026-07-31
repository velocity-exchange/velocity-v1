---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Generic relay cross-discovery for Custom quoters: `QuoterV0Account` gains the maker-declared reprice watch (`watchAccount`/`watchOffset`/`watchLen`, layout 2728→2760); new instructions `updateQuoterWatch`, `initializeQuoterCrossConditions` (per-entry `QuoterCrossConditionsV0` PDA), and the simulation-only `resolveCrankCrossMatchQuoter` in the IDL; relay resolvers renamed to `Resolve<EndpointName>` (`resolveCrankClobEvict`, `resolveCrankClobRemoveExpired`, `resolveCrankCrossMatch` — discriminators changed). Admin CLI adds `quoter set-watch` and `quoter attach-cross`.
