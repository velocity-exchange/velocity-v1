---
'@velocity-exchange/sdk': minor
---

`getPlaceAndTakePerpOrderIx`'s `clobAccounts` argument now builds the new
`placeAndTakePerpOrderV1` instruction rather than passing optional accounts to
`placeAndTakePerpOrder`. `placeAndTakePerpOrder`'s account list is frozen at its
pre-CLOB shape for ABI compatibility, so the CLOB route — where an unfilled
restable limit remainder rests on the book instead of being cancelled — is a
separate instruction with those accounts required. Callers that pass no
`clobAccounts` are unaffected.
