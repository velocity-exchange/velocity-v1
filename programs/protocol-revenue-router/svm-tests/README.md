# protocol-revenue-router svm-tests

LiteSVM tests that run the router's `distribute` against the real dfx-redemption program.

```bash
anchor build --ignore-keys -p protocol_revenue_router -- --features anchor-test
cargo test --manifest-path programs/protocol-revenue-router/svm-tests/Cargo.toml
```

## Redemption fixture

`fixtures/dfx_redemption.so` is a vendored build of `programs/dfx-redemption` from the
dfx-claim repo. Set `DFX_REDEMPTION_SO=<path>` to test against a different build. The tests
fail if the file is missing.

Refresh it whenever dfx-redemption changes, and refresh `idls/dfx_redemption.json` in the
same change so the CPI types match:

```bash
# in dfx-claim
anchor build --ignore-keys -p dfx_redemption
cp target/deploy/dfx_redemption.so <velocity-v1>/programs/protocol-revenue-router/svm-tests/fixtures/
cp target/idl/dfx_redemption.json <velocity-v1>/idls/dfx_redemption.json
```
