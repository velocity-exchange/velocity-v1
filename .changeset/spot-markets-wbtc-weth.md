---
'@velocity-exchange/sdk': minor
---

Add wBTC (spot market index 2) and wETH (index 3) to `MainnetSpotMarkets`. Both reuse the Pyth Lazer oracle accounts their perp counterparts already use (feed ids 1 and 2).

Do not publish this version until the two markets are initialized on chain. `findAllMarketAndOracles` derives `spotMarketIndexes` and oracle subscriptions from this registry when a client passes no explicit market list, so a client on a version that lists markets the chain does not have will try to subscribe to accounts that do not exist.
