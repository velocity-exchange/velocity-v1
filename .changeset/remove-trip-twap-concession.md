---
'@velocity-exchange/sdk': minor
---

Remove the equity-floor breaker's `$100` invalid-oracle dust concession. Invalid-oracle assets and perp longs now make a trip unprovable at every size; liabilities and shorts retain their sound zero upper bound. The program and SDK now derive observed equity, strict oracle validity, and the trip upper bound from one shared position walk. This removes the exported `EQUITY_FLOOR_TRIP_DUST_ALLOWANCE` constant without changing instructions, accounts, IDL, error codes, or strict floor gates.
