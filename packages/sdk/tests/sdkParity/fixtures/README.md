# Parity fixtures

Inputs and expected outputs for program math the SDK mirrors. The expected values come from the
program, and both sides assert against the same files, so a change to either implementation that
breaks parity fails a test.

- `calculate_spread.csv`, `apply_oracle_guard.csv` and `reference_price_offset.csv` cover
  `calculate_spread`, `apply_oracle_guard` and `calculate_reference_price_offset`. The program
  tests are in `vlp/amm/math/spread/tests.rs` (`parity_fixtures`) and the SDK tests in
  `tests/sdkParity/ammSpread.test.ts`.
- `adjust_amm.csv` covers `calculate_optimal_peg_and_budget` and `adjust_amm`, including the
  formulaic k decrease. The program test is in `vlp/amm/math/repeg/tests.rs` (`parity_fixtures`)
  and the SDK test in `tests/sdkParity/adjustAmm.test.ts`.

When the program's math changes on purpose, regenerate the expected columns from the program and
review the diff before committing it.
