---
'@velocity-exchange/admin-cli': minor
---

Add `fees set-schedule` command: rewrite the perp fee schedule in one instruction — taker fee in bps for the three live tiers (unused tiers mirror tier 2), with options to set the maker rebate, referrer/referee percentages, and the amm/if fee split in the same update. Fetches the current structure and patches only what is passed.
