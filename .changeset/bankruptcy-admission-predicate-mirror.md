---
'@velocity-exchange/sdk': patch
---

`isUserBankrupt` now mirrors the program's two value-aware bankruptcy vetoes (OtterSec #151/#145), so a keeper stops reporting an account as solvent that the program will resolve. A spot deposit row vetoes only when it is worth at least one token — a fully socialized market floors `cumulativeDepositInterest` at 1 and leaves every wiped depositor a positive `scaledBalance` worth nothing, which cannot be seized and previously blocked admission forever. A positive perp `quoteAssetAmount` vetoes only while its market's PnL pool can pay part of it, plus a new net-quote gate that keeps a net solvent estate out of bankruptcy however unfundable its claims. The exported signature is unchanged, but the function now reads market state as well as the user account and throws if a market referenced by a nonzero position is not loaded on the client.
