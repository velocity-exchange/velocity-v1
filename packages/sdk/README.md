<div align="center">
  <img width="180" height="180" src="https://docs.velocity.exchange/assets/velocity.svg" />

  <h1 style="margin-top:20px;">Velocity Exchange</h1>

  <p>
    <a href="https://www.npmjs.com/package/@velocity-exchange/sdk"><img alt="SDK npm package" src="https://img.shields.io/npm/v/@velocity-exchange/sdk" /></a>
    <a href="https://docs.velocity.exchange/developers"><img alt="Docs" src="https://img.shields.io/badge/docs-developers-blueviolet" /></a>
    <a href="https://discord.com/invite/95kByNnDy5"><img alt="Discord Chat" src="https://img.shields.io/discord/849494028176588802?color=blueviolet" /></a>
    <a href="https://opensource.org/licenses/Apache-2.0"><img alt="License" src="https://img.shields.io/github/license/velocity-exchange/velocity-v1?color=blueviolet" /></a>
  </p>
</div>

TypeScript client for [Velocity Protocol](https://docs.velocity.exchange), a perpetuals
and spot exchange on Solana. Read market state, place and manage orders, track positions
and margin, and run keeper or market-making infrastructure.

## Install

```bash
npm i @velocity-exchange/sdk
```

### Requirements

- **Node 20.18.0 or later.**
- **CommonJS only.** The package ships a `module` field, but it points at a
  browser-shimmed CommonJS build rather than ESM. Import it with `require`, or from
  TypeScript and bundlers configured for interop.
- **Anchor and web3.js are bundled, not peer dependencies.** The SDK depends on pinned
  versions directly: `@coral-xyz/anchor` aliased to `@anchor-lang/core@1.0.1`, a second
  `@coral-xyz/anchor-29` for legacy paths, and `@solana/web3.js@1.99.0`. When your app
  installs its own copy of either one, expect two copies in the tree and structurally
  identical types that TypeScript treats as distinct.

## Quickstart

Print the SOL-PERP vAMM bid/ask and oracle price. Needs no funds and signs nothing.
Full file: [`examples/read-market.ts`](./examples/read-market.ts).

```typescript
import { Connection, Keypair } from '@solana/web3.js';
import {
	BN,
	BulkAccountLoader,
	PerpMarkets,
	PRICE_PRECISION,
	VelocityClient,
	VelocityEnv,
	Wallet,
	calculateBidAskPrice,
	convertToNumber,
} from '@velocity-exchange/sdk';

const env: VelocityEnv = 'mainnet-beta';

const connection = new Connection(
	process.env.RPC_URL ?? 'https://api.mainnet-beta.solana.com',
	'confirmed'
);

// VelocityClient always requires a wallet, even on a read-only path. This
// throwaway keypair is never used to sign.
const wallet = new Wallet(Keypair.generate());

const client = new VelocityClient({
	connection,
	wallet,
	env,
	accountSubscription: {
		type: 'polling',
		accountLoader: new BulkAccountLoader(connection, 'confirmed', 1000),
	},
});
// subscribe() resolves false rather than throwing when a subscription fails.
if (!(await client.subscribe())) {
	throw new Error('failed to subscribe to Velocity accounts');
}

const solMarket = PerpMarkets[env].find((m) => m.baseAssetSymbol === 'SOL');
if (!solMarket) {
	throw new Error('SOL-PERP not listed on this deployment');
}
const marketIndex = solMarket.marketIndex;

const perpMarket = client.getPerpMarketAccountOrThrow(marketIndex);
const slot = await connection.getSlot();
const mmOracle = client.getMMOracleDataForPerpMarket(marketIndex, slot);

const [bid, ask] = calculateBidAskPrice(
	perpMarket.amm,
	perpMarket.marketStats,
	mmOracle,
	true,
	new BN(slot),
	client.getStateAccount()
);

console.log(`vAMM bid: $${convertToNumber(bid, PRICE_PRECISION)}`);
console.log(`vAMM ask: $${convertToNumber(ask, PRICE_PRECISION)}`);
```

### Placing an order

Continuing from the `client` and `marketIndex` above, but on devnet and with a
wallet that already has an initialized Velocity account with collateral. Full
runnable file: [`examples/place-order.ts`](./examples/place-order.ts).

```typescript
import {
	BASE_PRECISION,
	BN,
	PositionDirection,
	getMarketOrderParams,
} from '@velocity-exchange/sdk';

const txSig = await client.placePerpOrder(
	getMarketOrderParams({
		marketIndex,
		direction: PositionDirection.LONG,
		baseAssetAmount: new BN(1).mul(BASE_PRECISION),
	})
);
```

Both examples are typechecked in CI, so they stay in step with the API.

## What is inside

| Export | Reach for it when |
| --- | --- |
| `VelocityClient` | Anything that touches the exchange: reading markets, placing and cancelling orders, deposits and withdrawals. The entry point. |
| `User` | You need one account's positions, orders, collateral, health, or liquidation price. |
| `accountSubscription` modes | You are choosing how state reaches you. See below. |
| `dlob/` | You want the order book itself: resting orders, crossing logic, book levels, `DLOBSubscriber`. |
| `math/` | You need to predict an on-chain result off-chain: margin, funding, fees, AMM pricing, auctions, liquidation. |
| `events/` | You want to stream or backfill fills, funding payments, liquidations, and other program events. |
| `swift/` | You are placing signed-message orders rather than sending transactions yourself. |
| `tx/`, `priorityFee/` | You need control over transaction sending: retry strategy, priority fees, compute budget. |

### Subscription modes

`accountSubscription` accepts three types. The choice sets both your latency and your RPC
bill.

- **`polling`**: a `BulkAccountLoader` batches `getMultipleAccounts` on an interval. It
  works against any plain RPC with no extra infrastructure. Start here.
- **`websocket`**: account subscriptions pushed by the RPC. Lower latency than polling, at
  the cost of handling resubscription on an unreliable connection.
- **`grpc`**: a Yellowstone gRPC stream. The lowest latency, and what keepers and market
  makers run. It needs a gRPC endpoint that most public RPCs do not offer.

### Signed-message (Swift) orders

`placePerpOrder` builds and sends a transaction. The SDK can also place an order by
signing an off-chain message that a keeper then lands on chain. You give up direct control
of the transaction and gain a faster path into the auction without paying for your own
blockspace, which matters most for takers competing on fill quality. See the
[Swift docs](https://docs.velocity.exchange/developers/velocity-sdk/swift).

## BN and precision

Solana token amounts and prices need more precision than a JavaScript float can hold, so
every numeric value in this SDK is a [BN](https://github.com/indutny/bn.js) integer
scaled by a fixed precision.

A BN of `10,500,000` at precision `10^6` means `10.5`, because `10,500,000 / 10^6 = 10.5`.

| Precision constant | Value |
| --- | --- |
| `FUNDING_RATE_BUFFER_PRECISION` | 10^3 |
| `QUOTE_PRECISION` | 10^6 |
| `PEG_PRECISION` | 10^6 |
| `PRICE_PRECISION` | 10^6 |
| `AMM_RESERVE_PRECISION` | 10^9 |
| `BASE_PRECISION` | 10^9 |

BN division truncates, so converting back to a JavaScript number by dividing will
silently lose the fractional part. Always use `convertToNumber`:

```typescript
import { BN, convertToNumber } from '@velocity-exchange/sdk';

new BN(10500).div(new BN(1000)).toNumber(); // 10, which is wrong
convertToNumber(new BN(10500), new BN(1000)); // 10.5
```

Keep values as BN for as long as possible and convert only for display. See
[precision and types](https://docs.velocity.exchange/developers/velocity-sdk/precision-and-types).

## Relationship to the on-chain program

This SDK is a hand-maintained mirror of the Velocity program. The account layouts in
`types.ts` track the program's structs, and `math/` re-implements the program's pricing,
margin, funding, and fee logic in TypeScript, so you can predict on-chain results before
you send a transaction.

That mirroring is why the SDK version matters. An SDK older than the deployed program can
leave you with stale layouts or stale math. The symptom is a wrong answer rather than an
error: a mispredicted fill, margin, liquidation price, or funding payment. Track the
current release.

## Links

- [Developer docs](https://docs.velocity.exchange/developers): guides, API reference, and the Data API
- [Migrating from Drift](https://docs.velocity.exchange/developers/migrate-from-drift)
- [Discord](https://discord.com/invite/95kByNnDy5): `#research-and-dev-chat`
- [`velocity-rs`](https://docs.velocity.exchange/developers/velocity-rs), the Rust client

## Working in this repo

```bash
bun install                                          # once, at the repo root
bunx turbo run build --filter=@velocity-exchange/sdk
cd packages/sdk && bun run test:ci
```

See the root [`CLAUDE.md`](../../CLAUDE.md) and [`ARCHITECTURE.md`](../../ARCHITECTURE.md)
for the build, IDL, and SDK-mirror rules.

## License

Velocity Protocol v1 is licensed under [Apache 2.0](../../LICENSE).

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Velocity SDK by you, as defined in the Apache-2.0 license, shall be
licensed as above, without any additional terms or conditions.

Release history: [CHANGELOG.md](./CHANGELOG.md).
