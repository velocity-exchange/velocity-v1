---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

Fix warm-gated spot-market admin commands failing when proposed through the warm-admin Squads multisig. `getUpdateSpotMarketStatusIx`, `getUpdateWithdrawGuardThresholdIx`, `getUpdateSpotMarketIfFactorIx`, and `getUpdateSpotMarketScaleInitialAssetWeightStartIx` now accept an optional `admin` override, and `velocity-admin spot-market` commands resolve the admin signer to the executing authority (the multisig's vault 0 PDA with `--multisig`, else the local keypair) instead of always embedding `state.coldAdmin`.
