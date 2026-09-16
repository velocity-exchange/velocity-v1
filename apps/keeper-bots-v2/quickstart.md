# Quick start

A JIT market maker on Velocity fills taker orders during JIT auctions and earns the maker rebate on
the fills it wins. This is the shortest path from a clean checkout to a running `jitMaker` bot.

## 1. Create and fund an account on Velocity

Start on devnet while you are still changing the strategy. On mainnet the bot trades real
collateral and holds whatever inventory its fills leave it with.

- [Initialize the user](./README.md#initialize-the-user) and deposit collateral
- Background reading: https://docs.velocity.exchange/developers

## 2. Get a private RPC endpoint

The bot has to submit transactions to Solana validators through an RPC node, and public nodes rate
limit hard enough to cost you auctions. [Helius](https://dashboard.helius.dev/) has a free tier
that is enough to start. Closer colocation and a higher rate limit both help you win more fills.

## 3. Read `src/bots/jitMaker.ts`

This is the strategy, and it is meant to be edited.

- It quotes at the current top of book in the DLOB and sizes its maximum position to stay within
  `targetLeverage`, which defaults to 1 and is divided across the markets a subaccount makes
- It can make perp or spot markets, selected by the `marketType` field in the bot config
- It can skip specific counterparties through the jitter's user filter

## 4. Set your parameters in `jitMaker.config.yaml`

Choose which markets to quote, and which subaccount quotes each one. `subaccounts` and
`perpMarketIndicies` are matched position by position, and every subaccount used by the bot must
also be listed under `global.subaccounts`.

## 5. Run it

```shell
bun run dev --config-file=jitMaker.config.yaml
```

Prometheus metrics are on `localhost:9464/metrics`. Watch fill rate and inventory there before you
change anything.

## 6. Ask questions or send a patch

Join the [Discord](https://discord.com/invite/95kByNnDy5) for technical help, or open a pull
request against this repo.
