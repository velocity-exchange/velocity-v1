---
'@velocity-exchange/vaults-sdk': patch
---

Regenerated the vaults IDL and its types against the current velocity program. The embedded
`UserStats` type now exposes `accelerated_referral_status`, which the program carved out of former
padding, so every other field decodes the same as before. Several doc comments also match the
program again.
