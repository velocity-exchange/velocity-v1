---
'@velocity-exchange/sdk': minor
---

Add ZEC to the mainnet market registries.

`MainnetSpotMarkets` gains ZEC at spot index 4 (mint
`A7bdiYdS5GjqGFtxf17ppRHtDKPkkRqbKtR27dxvQXaS`, 8 decimals) and `MainnetPerpMarkets` gains
ZEC-PERP at perp index 4. Both use Pyth Lazer feed 66 and so share one oracle PDA,
`AqpaPcu6PYHrYNySrVptnQnwxVCNxWVFuCgCsr8R1eLQ`, the way wBTC and wETH share their perp feeds.

Nothing updates these registries automatically. Someone edits them by hand to match on-chain
state. dlob-server, keeper-bots-v2 and the relayer build their subscription lists from them, and
the relayer takes its Lazer feed set from `PerpMarkets`, so none of them can see a market that is
missing here. Ship this after the markets exist on chain and before redeploying those services.
