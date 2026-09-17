<div align="center">
  <img height="120x" src="https://uploads-ssl.webflow.com/611580035ad59b20437eb024/616f97a42f5637c4517d0193_Logo%20(1)%20(1).png" />

  <h1 style="margin-top:20px;">Keeper bots for Velocity Protocol v1</h1>

  <p>
    <a href="https://docs.velocity.exchange/developers/trading-automation/keeper-bots"><img alt="Docs" src="https://img.shields.io/badge/docs-developers-blueviolet" /></a>
    <a href="https://discord.com/invite/95kByNnDy5"><img alt="Discord Chat" src="https://img.shields.io/discord/849494028176588802?color=blueviolet" /></a>
    <a href="https://opensource.org/licenses/Apache-2.0"><img alt="License" src="https://img.shields.io/github/license/project-serum/anchor?color=blueviolet" /></a>
  </p>
</div>

# Setting up

This repo has two main branches:

* `master`: active development. It may be unstable, and it runs on the `devnet` cluster
* `mainnet-beta`: stable, and it runs on the `mainnet-beta` cluster

## Set up the environment

### YAML config file

A `.yaml` file configures the bot setup. `example.config.yaml` is a commented example.

Run a bot by loading the config file:
```shell
bun run dev --config-file=example.config.yaml
```

The fields, their meanings, and their defaults:

| Field             | Type   | Description | Default |
| ----------------- | ------ | --- | --- |
| global            | object | global configs to apply to all running bots | - |
| global.endpoint   | string | RPC endpoint to use | - |
| global.wsEndpoint | string | (optional) Websocket endpoint to use | derived from `global.endpoint` |
| global.keeperPrivateKey  | string | (optional) The private key to use to pay/sign transactions | `KEEPER_PRIVATE_KEY` environment variable |
| global.initUser   | bool   | Set `true` to init a fresh userAccount | `false` |
| global.websocket  | bool   | Set `true` to run the selected bots in websocket mode if compatible| `false` |
| global.runOnce    | bool   | Set `true` to run only one iteration of the selected bots | `false` |
| global.debug      | bool   | Set `true` to enable debug logging | `false` |
| global.subaccounts | list  | (optional) Which subaccount IDs to load | `0` |
| enabledBots       | list   | list of bots to enable, matching key must be present under `botConfigs` | - |
| botConfigs        | object | configs for associated bots | - |
| botConfigs.<bot_type> | object | config for a specific <bot_type> | - |


### Install dependencies

Run this once at the repo root. It installs every workspace dependency:
```shell
bun install
bun run build
```

## Initialize a user

A `User` account must exist before the bot interacts with the velocity program.

```shell
bun run dev --init-user
```

You can also load the private key into a browser wallet and initialize the user through the UI at https://app.velocity.exchange.

## Collateral

Some bots, such as the trading and liquidator bots, need collateral to keep positions open. A helper deposits it.
Initialize the user before you deposit collateral.

```shell
# deposit 10,000 of spot market 0's token
bun run dev --force-deposit 10000
```

You can also load the private key into a browser wallet and deposit collateral through the UI at https://app.velocity.exchange.

Free collateral determines the size of the borrows and perp positions an account can hold. Free collateral is total collateral minus the initial margin requirement. Total collateral is the value of the spot assets in the account plus the unrealized perp PnL. The initial margin requirement is the total weighted value of the perp positions and spot liabilities in the account. The [margin documentation](https://docs.velocity.exchange/protocol/trading/margin) gives the initial margin requirement weights. In short, free collateral is the part of total collateral that the borrows, the existing perp positions and the open orders do not use up.

# Run the bots

After you create the `config.yaml` file above, run:

```shell
bun run dev --config-file=config.yaml
```

By default some [Prometheus](https://prometheus.io/) metrics are exposed on `localhost:9464/metrics`.

# Notes on some bots

## Filler bot

Include `filler`, `spotFiller`, or both under `.enabledBots` in `config.yaml`. For a lightweight
filler for perp markets, include `fillerLite` rather than `filler`. The lighter filler runs on public
RPCs for testing, but it is less stable.

Read [the orderbook and keepers documentation](https://docs.velocity.exchange/protocol/how-it-works/orderbook-and-keepers).

A filler matches crossing orders on the exchange for a small cut of the taker fees. Fillers keep a
copy of the DLOB so they can find orders that cross. A filler also tries to execute triggerable
orders.

### Common errors

A filler bot can log these error codes in the transaction logs when pre-flight simulation fails:

#### For perps

| Error             | Description |
| ----------------- | ------ |
| OrderDoesNotExist | Outcompeted. Someone else already filled the order |
| OrderNotTriggerable | Outcompeted. Someone else already triggered the order |
| RevertFill | Outcompeted. Someone else already filled the order |


#### Other messages

| Message | Description |
| --------|--------------|
| filler last active slot != current slot | This can appear when another filler wins the fill. The *filler last active slot* is the last slot in which the filler filled an order, so it diverges from the *current slot* when the filler has not landed one since. |


## Liquidator bot

The liquidator bot monitors spot and perp markets for bankrupt accounts, and attempts to liquidate positions according to the protocol's [liquidation process](https://docs.velocity.exchange/protocol/trading/liquidations).

### Notes on derisking

Set `disableAutoDerisking` to `true` to turn off the derisking loop. That suits a larger strategy
that accepts the risk at a price better than market, because the liquidation fee applies.

### Notes on configuring a subaccount

By default the liquidator tries to liquidate, and so inherit the risk of, endangered positions in
every market. Set `botConfigs.liquidator.perpMarketIndicies`,
`botConfigs.liquidator.spotMarketIndicies`, or both in the config file to restrict which markets it
liquidates. The account named in `global.subaccounts` is the active account.

`perpSubaccountConfig` and `spotSubaccountConfig` replace `perpMarketIndicies` and
`spotMarketIndicies`. They map a subaccount to a list of market indexes. Both fields take a JSON
string:

### An example `config.yaml`
```
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
That config liquidates perp markets 0 to 2 on subaccount 0, perp markets 3 to 12 on subaccount 1, and spot markets 0 to 2 on subaccount 0. It also uses Jupiter to derisk spot assets into USDC. Every subaccount named in `botConfigs` must also appear in the global config. For the example above:

```
global:
  ...
  subaccounts: [0, 1]
```

### Common errors

The liquidator can log these error codes in the transaction logs when pre-flight simulation fails:

| Error             | Description |
| ----------------- | ------ |
| SufficientCollateral | The target account holds enough collateral, so it cannot be liquidated |
| InvalidSpotPosition | Outcompeted. Someone else already liquidated that account's spot position |
| InvalidPerpPosition | Outcompeted. Someone else already liquidated that account's perp position |
