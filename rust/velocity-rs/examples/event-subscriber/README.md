# Event subscriber example

Shows how to use the `EventSubscriber`. It runs on websocket by default; pass `--grpc` to
use a Yellowstone gRPC datasource instead. It prints each order fill and exits once it has
seen more than 100 of them.

Run on mainnet
```shell
WS_RPC_ENDPOINT=wss://your-rpc-with-wss.com cargo run
```

or

```shell
GRPC_ENDPOINT=https://your-rpc-with-grpc.com:2053 GRPC_X_TOKEN=00000000-0000-0000-0000-000000000000 cargo run -- --grpc
```

Enable logging with `RUST_LOG=debug`.

Alternatively use a `.env` file.
