# Market maker example

Places and cancels fixed limit and floating (oracle offset) limit orders on sol-perp, using
the wallet's sub-account 0, and requotes every 400ms. `RPC_URL` defaults to
`https://api.mainnet-beta.solana.com`.

Run mainnet WebSocket example
```shell
PRIVATE_KEY="<base58 private key>" \
MAINNET=1 \
RPC_URL="mainnet-rpc.example.com" \
 cargo run --release
```

Run mainnet gRPC example
```shell
PRIVATE_KEY="<base58 private key>" \
MAINNET=1 \
RPC_URL="mainnet-rpc.example.com" \
GRPC_URL="" \
GRPC_X_TOKEN="" \
 cargo run --release -- --grpc
```

Unset `MAINNET` to run against devnet.

## JIT making

Velocity MMs can also provide just in time (JIT) matching via swift and the jit-proxy helper
program. For examples see the `swift-maker` and `jitter` examples next to this one, and the
jit-proxy example: https://github.com/velocity-exchange/jit-proxy/tree/master/rust
