---
'@velocity-exchange/sdk': minor
---

Retune the vAMM top-of-book quote breakpoints to $250/$750/$2000/$5000 on both the default and majors ladders, and add `isMajorPerpMarket` / `MAJOR_PERP_MARKET_INDEXES` as one definition of major-market tiering for SDK consumers.

Consumers that read `DEFAULT_TOP_OF_BOOK_QUOTE_AMOUNTS` or `MAJORS_TOP_OF_BOOK_QUOTE_AMOUNTS` will see the retuned values on upgrade, which shifts near-touch level sizing on any locally derived vAMM book. `DLOBSubscriber.getL2` now selects between the two via `isMajorPerpMarket` instead of a `marketIndex < 3` literal, so market index 3 (HYPE) is no longer treated as a major.
