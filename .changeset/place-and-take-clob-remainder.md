---
'@velocity-exchange/sdk': minor
---

`place_and_take_perp_order` can rest an unfilled limit remainder on the CLOB: the instruction gained five optional trailing accounts (`quoter`, `clob_market`, `clob_program`, `velocity_signer`, `crank_conditions`); when passed and the order is a non-IOC limit, the remainder is cancelled off the DLOB and re-placed on the CLOB (margin gate re-run, wake hints folded, `OrderRef` in tx return data), degrading to today's cancel when the book is dead or margin fails. `buildPlaceAndTakePerpOrderInstruction` / `getPlaceAndTakePerpOrderIx` gained an optional `clobAccounts` arg; omitted optional accounts are encoded as the program id (Anchor's `None`), so instructions rebuilt through the SDK stay compatible automatically.
