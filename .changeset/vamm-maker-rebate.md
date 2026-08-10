---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Add the vAMM maker rebate feature flag. New onchain `FeatureBitFlags::VammMakerRebate` (bit 8, off by default): when enabled, the vAMM earns the maker rebate on fills it makes against a taker, carved off the taker-fee remainder before the protocol/IF/AMM split and folded into the AMM's fee provision. The taker's fee is unchanged; only the distribution shifts. SDK: `FeatureBitFlags.VAMM_MAKER_REBATE`, `AdminClient.updateFeatureBitFlagsVammMakerRebate` / `getUpdateFeatureBitFlagsVammMakerRebateIx`. Admin CLI: `velocity-admin feature-flags vamm-maker-rebate <true|false>`.
