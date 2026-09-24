<div align="center">
  <img height="120" src="https://docs.velocity.exchange/assets/velocity.svg" />

  <h1>Keeper bots</h1>

  <p>
    <a href="https://docs.velocity.exchange/developers/trading-automation/keeper-bots"><img alt="Docs" src="https://img.shields.io/badge/docs-developers-blueviolet" /></a>
    <a href="https://discord.com/invite/95kByNnDy5"><img alt="Discord Chat" src="https://img.shields.io/discord/849494028176588802?color=blueviolet" /></a>
    <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blueviolet" /></a>
  </p>
</div>

A single process that loads one or more keeper bots against Velocity Protocol: the liquidator,
settlers, and cranks. You pick which bots to run in a config file,
and every bot in the process shares one `VelocityClient` and one wallet.

The repo has two long-lived branches. `master` is the bleeding edge and may be unstable.
`mainnet-beta` is the stable branch.

# Setting up

## Config file

Bots are configured with a `.yaml` file. `example.config.yaml` is a commented reference for the
global options and the common bot configs. `lite.config.yaml` is a smaller starting point.

Load a config file with `--config-file`:

```shell
bun run dev --config-file=example.config.yaml
```

Without `--config-file`, the process builds its config from command-line flags instead. Run
`bun run dev --help` for the full flag list.

The top-level fields:

| Field                 | Type   | Description                                                             | Default                                 |
| --------------------- | ------ | ----------------------------------------------------------------------- | --------------------------------------- |
| global                | object | Global config applied to every bot in the process                       | -                                       |
| global.velocityEnv    | string | Cluster to connect to, `devnet` or `mainnet-beta`                       | `ENV` environment variable, or `devnet` |
| global.endpoint       | string | RPC endpoint to use                                                     | `ENDPOINT` environment variable         |
| global.wsEndpoint     | string | Websocket endpoint to use                                               | derived from `global.endpoint`          |
| global.keeperPrivateKey | string | Private key used to pay for and sign transactions                     | `KEEPER_PRIVATE_KEY` environment variable |
| global.initUser       | bool   | Set `true` to create the user account when it does not exist            | `false`                                 |
| global.forceDeposit   | number | Deposit this many of spot market 0's token, then exit                   | unset                                   |
| global.websocket      | bool   | Set `true` to run the selected bots in websocket mode where supported   | `false`                                 |
| global.runOnce        | bool   | Set `true` to run one iteration of the selected bots and exit           | `false`                                 |
| global.debug          | bool   | Set `true` to enable debug logging                                      | `false`                                 |
| global.subaccounts    | list   | Subaccount IDs to load                                                  | `[0]`                                   |
| global.metricsPort    | number | Port for the Prometheus exporter                                        | `9464`                                  |
| enabledBots           | list   | Bots to enable. Each entry needs a matching key under `botConfigs`      | `[]`                                    |
| botConfigs            | object | Per-bot config, keyed by bot type                                       | `{}`                                    |

Unknown keys in the file are carried through without being read, so a typo fails silently. Check
the field against `src/config.ts` if a setting appears to have no effect.

## Install dependencies

Install once at the repo root, then build this app:

```shell
bun install
bunx turbo run build --filter=@velocity-exchange/keeper-bots-v2
```

The `bun run` commands in this file run from `apps/keeper-bots-v2`.

## Initialize the user

Bots that hold positions or collateral, such as the liquidator, need a Velocity user account for the signing wallet. The process throws at startup if the account is missing, so
create it first:

```shell
bun run dev --init-user
```

This calls `VelocityClient.initializeUserAccount()` for the active subaccount. You can instead load
the private key into a browser wallet and initialize the account through the UI at
https://app.velocity.exchange.

## Collateral

The same bots need collateral to keep positions open. A user account must exist before you can
deposit.

```shell
# deposit 10,000 of spot market 0's token
bun run dev --force-deposit 10000
```

The deposit goes to spot market index 0, which is USDT on mainnet and dUSDT on devnet. On devnet the process mints the amount from the
token faucet first. It exits once the deposit transaction is sent, so run it again without the flag
to start the bot. You can also deposit through the UI at https://app.velocity.exchange.

Free collateral determines the size of the borrows and perp positions an account can carry. It is
total collateral minus the initial margin requirement. Total collateral is the value of the spot
assets in the account plus unrealized perp PnL. The initial margin requirement is the total
weighted value of the perp positions and spot liabilities in the account, weighted as described in
the [margin documentation](https://docs.velocity.exchange/protocol/trading/margin).

# Run bots

With a config file in place:

```shell
bun run dev --config-file=config.yaml
```

[Prometheus](https://prometheus.io/) metrics are exposed on `localhost:9464/metrics` by default.
Override the port with `global.metricsPort`, or turn metrics off with `global.disableMetrics`.

# Notes on some bots

## Order filling

There is no filler bot. Velocity matches on an on-chain CLOB reached through a router, so a
taker's counterparties come from the books rather than from whoever assembles the transaction.
The permissionless cross cranks (`crank_cross_match`, `crank_taker_origin_cross`) close a crossed
book, and relay's trigger resolvers fire an armed trigger order.

## Liquidator bot

The liquidator watches spot and perp markets for accounts whose collateral has fallen below their
maintenance margin requirement, and takes over their positions following the protocol's
[liquidation process](https://docs.velocity.exchange/protocol/trading/liquidations). It also
resolves bankruptcy on accounts that are past that point. Taking over a position transfers that
position's risk to the liquidator's own account.

A perp liquidation reverts while the account holds resting CLOB orders. Set
`botConfigs.liquidator.dlobServerHttpUrl` to a dlob-server base URL, and the liquidator reads the
account's resting orders from it and force-cancels them first. When it is unset, the liquidator
skips the force-cancel.

### Derisking

The liquidator closes inherited positions by default. Set `disableAutoDerisking` to `true` to turn
that loop off, for example if you want to hold the inherited risk and unwind it yourself at a price
you choose. It will also route spot assets through Jupiter to derisk them into USDC.

### Configuring subaccounts

By default the liquidator tries to liquidate endangered positions in every market. Use
`botConfigs.liquidator.perpSubAccountConfig` and `spotSubAccountConfig` to restrict it, mapping
each subaccount ID to the market indexes it should liquidate.

The flat `perpMarketIndicies` and `spotMarketIndicies` fields do the same job for a single
subaccount, the one named in `global.subaccounts`. Both are deprecated in favor of the
per-subaccount maps.

### An example `config.yaml`

```yaml
botConfigs:
  ...
  liquidator:
    ...
    perpSubAccountConfig:
      0:
        - 0
        - 1
        - 2
      1:
        - 3
        - 4
        - 5
        - 6
        - 7
        - 8
        - 9
        - 10
        - 11
        - 12
    spotSubAccountConfig:
      0:
        - 0
        - 1
        - 2
```

That liquidates perp markets 0 to 2 on subaccount 0, perp markets 3 to 12 on subaccount 1, and
spot markets 0 to 2 on subaccount 0. Every subaccount named in `botConfigs` must also be listed in
`global.subaccounts`, because subaccounts are loaded before the bots are constructed:

```yaml
global:
  ...
  subaccounts: [0, 1]
```

### Common errors

| Error                | Description                                                             |
| -------------------- | ----------------------------------------------------------------------- |
| SufficientCollateral | The account is above the liquidation threshold and cannot be liquidated |
| InvalidSpotPosition  | Outcompeted, the account's spot position was already liquidated         |
| InvalidPerpPosition  | Outcompeted, the account's perp position was already liquidated         |
