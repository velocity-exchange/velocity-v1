---
'@velocity-exchange/sdk': minor
---

Relay conditions are one account per user. `getLiqConditionsPublicKey` and
`getTriggerConditionsPublicKey` are replaced by `getUserConditionsPublicKey`
(PDA `["user_conditions", user]`), and `getRelayScratchPublicKey` derives the
new program-wide resolver staging account. `initializeUser` now passes the
user-conditions account by default, so new users are relay-covered from birth;
callers building the instruction by hand must name it (pass the PDA, or `null`
to decline the rent).
