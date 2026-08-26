---
'@velocity-exchange/sdk': minor
---

`PythClient.getOraclePriceDataFromBuffer` checks the pyth v2 price account header before it
decodes. The buffer must be at least 3312 bytes, the length of a price account, and must carry
magic `0xa1b2c3d4`, version 2, and account type 3 (`AccountType::Price`). Otherwise the call
throws.

This mirrors the program, which now rejects the same accounts. Ownership by the pyth program does
not make an account a price feed — that program also owns mapping accounts and product accounts,
and the push-oracle decoder reinterprets whatever bytes it is given. A decoded number for such an
account is worse than an error, because a caller cannot tell it apart from a price.
