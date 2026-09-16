# VelocityClient callback subscriptions example

Subscribes to every perp market with a callback and prints each market's
`base_asset_amount_with_amm` as updates arrive. It covers deserializing market account
data, reading AMM fields, and where the callback API bites.

## Usage

Mainnet, with the default RPC:

```bash
cd examples/velocity-client-callbacks
cargo run
```

With a custom RPC endpoint:

```bash
RPC_URL=https://your-rpc-endpoint.com cargo run
```

Other options:

```bash
# Run for 60 seconds (default 30)
cargo run -- --duration 60

# Devnet instead of mainnet
cargo run -- --devnet

# Debug logging
RUST_LOG=debug cargo run
```

`--rpc-url` sets the endpoint too, and `RPC_URL` overrides it when both are present. The
client converts the http url to a websocket one for the subscriptions.

## Callback implementation

Subscribing to market updates:

```rust
// Subscribe to perp markets with a callback
client.subscribe_markets_with_callback(&markets, |update| {
    // Process market update
    process_market_update(update);
}).await?;
```

Processing market data:

```rust
// Callback to process market updates
let callback = move |update: &AccountUpdate| {
    // Deserialize PerpMarket from account data
    match deserialize_perp_market(&update.data) {
        Ok(market) => {
            println!(
                "Market {}: base_asset_amount_with_amm = {}",
                market.market_index,
                market.amm.base_asset_amount_with_amm
            );
        }
        Err(e) => {
            eprintln!("Failed to deserialize market: {}", e);
        }
    }
};
```

## Gotchas

**Account data deserialization.** Decode zero-copy accounts (`PerpMarket`, `SpotMarket`,
`User`, …) with the SDK's alignment-safe readers:
`velocity_rs::utils::try_deser_zero_copy::<T>(data)` (or `deser_zero_copy`), or
`client.get_account::<T>(..)` and the account-map getters. Do **not** call anchor's
`T::try_deserialize` on raw account bytes. For 16-byte-aligned accounts, meaning those with
`u128`/`i128` fields such as `PerpMarket` and `SpotMarket`, it casts by reference and panics
on the byte-aligned `Vec<u8>` an update carries (`from_bytes` gives
`TargetAlignmentGreaterAndInputNotAligned`). Handle decode errors instead of unwrapping;
not every update is a valid account.

**Callback lifetime and state.** The callback must be `Fn + Send + Sync + Clone + 'static`,
so it captures state by value. Share mutable state through `Arc<Mutex<..>>`.

**Subscription management.** One callback is registered per market: subscribing to a market
the client already holds a subscription for skips it, so the second callback never fires.
These methods also return `AlreadySubscribed` once the client is subscribed over gRPC.
Subscriptions reconnect and replay themselves after a network drop. Call
`client.unsubscribe()` when you are done to release the subscriptions.

**Performance.** Callbacks run on the subscription task and block it while they run, so keep
them short, push heavy computation onto another task, and batch updates when processing is
expensive.
