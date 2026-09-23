# @velocity-exchange/vaults-sdk

TypeScript SDK for the Velocity vaults program (`vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR`).
`VaultClient` is the entry point: it creates and manages vaults, handles depositor accounts, and
reads vault state. The package also ships the CLI documented below.

```bash
npm i @velocity-exchange/vaults-sdk
```

## CLI

Run the CLI from this package directory, which is where its `package.json` lives:

```sh
bun run cli --help
```

Add `--help` to any subcommand to see its options.

The CLI needs an RPC node and a keypair to sign transactions. Supply both as environment
variables, in a `.env` file, or as command line flags.

| Environment variable | Flag         | Description                                                                                                                 |
| -------------------- | ------------ | --------------------------------------------------------------------------------------------------------------------------- |
| `RPC_URL`            | `--url`      | The RPC node to connect to for transactions. Required                                                                        |
| `KEYPAIR_PATH`       | `--keypair`  | Path to a keypair, as a file or a base58 string. A ledger path also works, for example `usb://ledger/<wallet_id>?key=0/0`     |
| `ENV`                | `--env`      | `devnet` or `mainnet-beta`. Defaults to `mainnet-beta`                                                                       |
| none                 | `--commitment` | State commitment to use. Defaults to `confirmed`                                                                           |

## Manager commands

Vault managers run these. Point `KEYPAIR_PATH` at the manager's keypair.

### Initialize a new vault

`init-vault` creates a vault and makes you, the manager, its delegate unless you pass
`--delegate`.

```sh
bun run cli init-vault --name <VAULT_NAME>
```

| Option | Description |
| --- | --- |
| `-n, --name <string>` | Name of the vault to create. Required |
| `-i, --market-index <number>` | Spot market index to accept for deposits. Default `0`, which is USDC |
| `-r, --redeem-period <number>` | Seconds a depositor must wait after requesting a withdraw. Default `604800`, which is 7 days |
| `-x, --max-tokens <number>` | Max spot `marketIndex` tokens the vault can accept. Default `0`, which is unlimited |
| `-m, --management-fee <percent>` | Annualized management fee charged to depositors. Default `0` |
| `-s, --profit-share <percent>` | Percentage of profits charged by the manager. Default `0` |
| `-p, --permissioned` | Make the vault permissioned, so the manager must initialize each vault depositor. Default off |
| `-a, --min-deposit-amount <number>` | Minimum token amount allowed per deposit. Default `0` |
| `-d, --delegate <publicKey>` | Address to make the delegate of the vault |
| `--manager <publicKey>` | The manager for the vault |
| `--dump-transaction-message` | Print the transaction message to the console |

### Update vault params

```sh
bun run cli manager-update-vault --vault-address=<VAULT_ADDRESS> [options]
```

| Option | Description |
| --- | --- |
| `--vault-address <address>` | Address of the vault to update. Required |
| `-r, --redeem-period <number>` | New redeem period. Can only be lowered |
| `-x, --max-tokens <number>` | Max tokens the vault can accept |
| `-a, --min-deposit-amount <number>` | Minimum token amount allowed per deposit |
| `-m, --management-fee <percent>` | New management fee. Can only be lowered here; use the timelocked `manager-update-fees` to raise it |
| `-s, --profit-share <percent>` | New profit share percentage. Can only be lowered here; use the timelocked `manager-update-fees` to raise it |
| `-h, --hurdle-rate <percent>` | New hurdle rate percentage. Can only be raised here; use the timelocked `manager-update-fees` to lower it |
| `-p, --permissioned <boolean>` | Set the vault as permissioned (`true`) or open (`false`) |
| `--dump-transaction-message` | Print the transaction message to the console |

### Enable margin trading

Trading spot on margin in the vault requires margin trading to be turned on first.

```sh
bun run cli manager-update-margin-trading-enabled --vault-address=<VAULT_ADDRESS> --enabled=<true|false>
```

### Manager deposit

Deposit into a vault as the manager. `DEPOSIT_AMOUNT` is in human precision, so `5` means 5 USDC.

```sh
bun run cli manager-deposit --vault-address=<VAULT_ADDRESS> --amount=<DEPOSIT_AMOUNT>
```

### Manager withdraw

Request a withdraw as the manager. `SHARES` is in raw precision.

```sh
bun run cli manager-request-withdraw --vault-address=<VAULT_ADDRESS> --amount=<SHARES>
```

Complete the withdraw once the redeem period has passed:

```sh
bun run cli manager-withdraw --vault-address=<VAULT_ADDRESS>
```

### Apply profit share

The manager can trigger a profit share calculation. It looks up every `VaultDepositor` on the
vault that is eligible for profit share and processes them in batches.

```sh
bun run cli apply-profit-share-all --vault-address=<VAULT_ADDRESS>
```

## Depositor commands

### Deposit into a vault

**Permissioned vaults.** The manager must initialize the `VaultDepositor` account before that
authority can deposit.

```sh
bun run cli init-vault-depositor --vault-address=<VAULT_ADDRESS> --deposit-authority=<AUTHORITY_TO_ALLOW_DEPOSIT>
```

**Permissionless vaults.** Anyone can deposit. The `deposit` instruction initializes a
`VaultDepositor` account if one does not already exist. `DEPOSIT_AMOUNT` is in human precision of
the deposit token, so `5` means 5 USDC.

```sh
bun run cli deposit --vault-address=<VAULT_ADDRESS> --deposit-authority=<DEPOSIT_AUTHORITY> --amount=<DEPOSIT_AMOUNT>
```

You can pass the `VaultDepositor` address directly instead:

```sh
bun run cli deposit --vault-depositor-address=<VAULT_DEPOSITOR_ADDRESS> --amount=<DEPOSIT_AMOUNT>
```

### Withdraw from a vault

Request a withdraw, which starts the redeem period:

```sh
bun run cli request-withdraw --vault-address=<VAULT_ADDRESS> --authority=<AUTHORITY> --amount=<WITHDRAW_AMOUNT>
```

Complete it once the redeem period has passed:

```sh
bun run cli withdraw --vault-address=<VAULT_ADDRESS> --authority=<AUTHORITY>
```

## Read-only commands

Print the current state of a `Vault`:

```sh
bun run cli view-vault --vault-address=<VAULT_ADDRESS>
```

Print the current state of a `VaultDepositor`:

```sh
bun run cli view-vault-depositor --vault-depositor-address=<VAULT_DEPOSITOR_ADDRESS>
```

## Working in this repo

The IDL and generated types under `src/idl/vaults.json` and `src/types/vaults.ts` come from
`programs/vaults`. Regenerate them with `bun run program:idl:vaults` at the repo root, and never
hand-edit them.
