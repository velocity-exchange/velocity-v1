---
'@velocity-exchange/sdk': patch
---

IF add zero-shares guard (High audit fix): `add_insurance_fund_stake` now rejects a positive deposit that would mint zero insurance-fund shares (new `IFDepositMintsZeroShares` error, 6360). Previously shares were computed off the pre-transfer, donation-inflatable vault balance with no nonzero-share check, so an attacker could donate into the vault before a victim's add to force `floor(amount * total_shares / vault) == 0` and capture the victim's full deposit as share-price appreciation. Mirrors the `n_shares > 0` guard the request-remove path already enforced. No account layout change; the regenerated IDL gains one error entry.
