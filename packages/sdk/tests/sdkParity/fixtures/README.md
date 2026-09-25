# Spread parity fixtures

Inputs and expected outputs for `calculate_spread`, `apply_oracle_guard` and
`calculate_reference_price_offset`. The expected values come from the program. Both the
program's tests (`vlp/amm/math/spread/tests.rs`, `parity_fixtures`) and the SDK's
(`tests/sdkParity/ammSpread.test.ts`) assert against these files, so a change to either
implementation that breaks parity fails a test.

When the program's spread math changes on purpose, regenerate the expected columns from the
program and review the diff before committing it.
