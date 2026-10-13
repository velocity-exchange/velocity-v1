# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `math::amm_quote::project_perp_market_for_quoting` takes the unix time `now` and first
  refreshes the market's oracle-derived stats (`PerpMarket::update_oracle_derived_stats`), as
  `fill_perp_order` does before quoting. The spread refresh reads those stats, so quotes
  projected without it could miss real crosses or report ones the program would not fill.

### Fixed

- The DLOB reports an order as crossing the vAMM when its size is at least one step, the
  smallest fill the program's AMM sizing produces. It used to require `min_order_size`, which
  the program applies only when an order is placed, so a partly filled order whose remainder
  fell below the minimum was never offered to the vAMM. Reduce-only orders no longer get an
  exemption: below one step the AMM fills nothing either way.
- `try_get_mmoracle_for_perp_market` ages the cached exchange oracle's delay to `current_slot`.
  The delay was the one cached with the last oracle update, so the safe oracle it returned
  looked fresher than the program would see it.

## [1.0.1] - 2026-07-05

### Fixed

- Corrected the `PYTH_LAZER_FEED_ID_TO_{PERP,SPOT}_MARKET_MAINNET` tables to Velocity's
  deployed market indices; they still carried the inherited Drift market numbering,
  causing keep-rs to subscribe to the wrong lazer feed for some markets (#203).
