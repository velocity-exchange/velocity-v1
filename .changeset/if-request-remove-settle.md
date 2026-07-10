---
'@velocity-exchange/sdk': patch
---

IF request-remove settle (High audit fix): `requestRemoveInsuranceFundStake` now settles already-due protocol revenue into the insurance-fund vault before freezing the staker's withdraw value, mirroring `addInsuranceFundStake`. Previously the exit value was frozen against the pre-settle vault, so a public revenue settle between request and remove shifted the exiting staker's rightful share of already-due revenue to the remaining stakers. The `requestRemoveInsuranceFundStake` instruction gains `state`, `spotMarketVault`, `velocitySigner`, and `tokenProgram` accounts (plus transfer-hook remaining accounts); the SDK builder supplies them automatically, but manual instruction construction must include them. No on-chain account layout change.
