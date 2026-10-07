# quoter-test-support

Litesvm harness shared by the CLOB and midpoint integration tests. Not published; `dev-dependency`
only.

## Dependents

- `anchor-v2/programs/clob` (dev-dependency)
- `anchor-v2/programs/midpoint` (dev-dependency)

## Build and test

Builds as part of the `anchor-v2` workspace; it has no tests of its own. It is exercised through
`bun run test:clob` and `bun run test:midpoint`.

## Design

Both suites drive a real `.so` over litesvm and read a quoter response out of the account the
return-data pointer names. That is the quoter interface, so the harness for it belongs to neither
program. Each suite keeps its own `Ctx`, because the keypairs a market needs are not the keypairs
a spline quoter needs.
