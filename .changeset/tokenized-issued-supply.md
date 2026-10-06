---
'@velocity-exchange/vaults-sdk': patch
---

The vaults IDL gains `TokenizedVaultDepositor.issuedSupply`, carved from padding, so the account
size is unchanged. The program now prices `tokenizeShares` and `redeemTokens` from this counter
rather than the mint's supply, so a holder who burns tokens directly through the SPL Token program
can no longer reprice the wrapper for everyone else. Tokens burned that way are no longer
redeemable for the shares behind them. When the last live token is redeemed, those shares go back
to all vault depositors pro rata.

The same IDL regeneration also picks up `UserStats.acceleratedReferralStatus`, which the velocity
program already had, and refreshes a few doc strings on shared velocity types.
