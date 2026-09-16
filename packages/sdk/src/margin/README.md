# Margin calculation snapshot

The SDK's margin engine computes one immutable snapshot per pass and has the `User` getters read
from it, instead of each getter re-walking the user's positions. The code lives in
`packages/sdk/src/marginCalculation.ts`; this directory holds only these notes.

## Alignment with the program

- The snapshot shape mirrors `MarginContext` and `MarginCalculation` in
  `programs/velocity/src/state/margin_calculation.rs`.
- The inputs and the order they are accumulated in mirror
  `calculate_margin_requirement_and_total_collateral_and_liability_info` in
  `programs/velocity/src/math/margin.rs`.
- Isolated perp positions live in `isolatedMarginCalculations`, keyed by perp `marketIndex`, matching
  the program's split between the cross-margin book and per-position isolated collateral.

Keep the math identical to the program: accumulation order, buffers, funding, open-order initial
margin, and oracle strictness. A rounding difference here shows up as an SDK that mispredicts
liquidation.

## Types

All BN amounts are `QUOTE_PRECISION` (1e6) unless noted. Buffers are `MARGIN_PRECISION` (1e4)
fractions of liability value.

```ts
import { BN } from './isomorphic/anchor';
import { MarketType } from './types';

// Re-exported from ./types. 'Fill' is the integer midpoint of Initial and Maintenance.
export type MarginCategory = 'Initial' | 'Maintenance' | 'Fill';

export type MarginCalculationMode = { type: 'Standard' } | { type: 'Liquidation' };

export class MarketIdentifier {
	marketType: MarketType;
	marketIndex: number;

	static spot(marketIndex: number): MarketIdentifier;
	static perp(marketIndex: number): MarketIdentifier;
	equals(other: MarketIdentifier | undefined): boolean;
}

export class MarginContext {
	marginType: MarginCategory;
	mode: MarginCalculationMode;
	strict: boolean;
	ignoreInvalidDepositOracles: boolean;
	isolatedMarginBuffers: Map<number, BN>;
	crossMarginBuffer: BN;

	// The constructor is private; build a context through one of these.
	static standard(marginType: MarginCategory): MarginContext;
	static liquidation(
		crossMarginBuffer: BN,
		isolatedMarginBuffers: Map<number, BN>
	): MarginContext;

	// Builders, each returning this for chaining.
	strictMode(strict: boolean): this;
	ignoreInvalidDeposits(ignore: boolean): this;
	setCrossMarginBuffer(crossMarginBuffer: BN): this;
	setIsolatedMarginBuffers(isolatedMarginBuffers: Map<number, BN>): this;
	setIsolatedMarginBuffer(marketIndex: number, isolatedMarginBuffer: BN): this;
}

export class IsolatedMarginCalculation {
	marginRequirement: BN;
	totalCollateral: BN; // deposit + pnl
	totalCollateralBuffer: BN;
	marginRequirementPlusBuffer: BN;

	getTotalCollateralPlusBuffer(): BN;
	meetsMarginRequirement(): boolean;
	meetsMarginRequirementWithBuffer(): boolean;
	marginShortage(): BN;
}

export class MarginCalculation {
	context: MarginContext;

	totalCollateral: BN;
	totalCollateralBuffer: BN;
	marginRequirement: BN;
	marginRequirementPlusBuffer: BN;

	isolatedMarginCalculations: Map<number, IsolatedMarginCalculation>;

	totalPerpLiabilityValue: BN;
	numSpotLiabilities: number;
	numPerpLiabilities: number;
	withPerpIsolatedLiability: boolean;
	withSpotIsolatedLiability: boolean;

	// Accumulators, called by User.getMarginCalculation as it walks positions.
	addCrossMarginTotalCollateral(delta: BN): void;
	addCrossMarginRequirement(marginRequirement: BN, liabilityValue: BN): void;
	addIsolatedMarginCalculation(
		marketIndex: number,
		depositValue: BN,
		pnl: BN,
		liabilityValue: BN,
		marginRequirement: BN
	): void;
	addPerpLiabilityValue(perpLiabilityValue: BN): void;
	addSpotLiability(): void;
	addPerpLiability(): void;
	updateWithSpotIsolatedLiability(isolated: boolean): void;
	updateWithPerpIsolatedLiability(isolated: boolean): void;

	// Cross margin
	getCrossTotalCollateralPlusBuffer(): BN;
	meetsCrossMarginRequirement(): boolean;
	meetsCrossMarginRequirementWithBuffer(): boolean;
	getCrossFreeCollateral(): BN;

	// Cross and isolated together
	meetsMarginRequirement(): boolean;
	meetsMarginRequirementWithBuffer(): boolean;
	getNumOfLiabilities(): number;

	// Isolated margin
	getIsolatedFreeCollateral(marketIndex: number): BN;
	getIsolatedMarginCalculation(marketIndex: number): IsolatedMarginCalculation | undefined;
	hasIsolatedMarginCalculation(marketIndex: number): boolean;
}
```

`meetsMarginRequirement` and `meetsMarginRequirementWithBuffer` on `MarginCalculation` require the
cross book and every tracked isolated position to pass independently.
`getIsolatedFreeCollateral(marketIndex)` throws `InvalidMarginCalculation: missing isolated calc` when
no isolated calculation was recorded for that market, while `getIsolatedMarginCalculation` returns
`undefined` for the same case.

`numSpotLiabilities`, `numPerpLiabilities`, `withPerpIsolatedLiability` and
`withSpotIsolatedLiability` back the program's `validate_any_isolated_tier_requirements` rule, which
is about a market's `ContractTier::Isolated` classification and not about per-position isolated
margin.

`MarketIdentifier` mirrors the on-chain identifier type. Nothing in the SDK passes one today.

## When the snapshot is computed

`getMarginCalculation(...)` computes the snapshot on the spot. Nothing recomputes it on account or
oracle updates, because oracle prices can move every slot and most of those recomputations would be
thrown away. Callers pick their own cadence; a UI driving an active trade form can call it about once
a second.

## User integration

```ts
public getMarginCalculation(
  marginCategory: MarginCategory = 'Initial',
  opts?: {
    strict?: boolean;                                 // TWAP-bounded StrictOraclePrice pricing
    includeOpenOrders?: boolean;                      // defaults to true
    liquidationBufferMap?: Map<number | 'cross', BN>; // 'cross' or a perp market index
  }
): MarginCalculation;
```

The pass builds a `MarginContext.standard(marginCategory)` and moves `liquidationBufferMap` into it:
the `'cross'` entry becomes `crossMarginBuffer` and every numeric key becomes that market's isolated
buffer. It throws `InvalidPoolId: ...` when a position's market pool id does not match the user's
`poolId`, with one carve-out mirroring `margin.rs`: a pool-1 user may hold a quote deposit from pool
0, and that deposit contributes zero collateral.

These getters call it rather than walking positions themselves:

- `getTotalCollateral()` reads `totalCollateral`, or the isolated bucket's collateral when a
  `perpMarketIndex` is passed, throwing if that market has no isolated calculation.
- `getMarginRequirement()` reads `marginRequirement`, or `marginRequirementPlusBuffer` when a
  liquidation buffer is supplied, returning `ZERO` for an absent isolated bucket.
- `getInitialMarginRequirement()` and `getMaintenanceMarginRequirement()` wrap
  `getMarginRequirement()` with the strict-pricing and open-order defaults each category uses.
- `getFreeCollateral()` reads `getCrossFreeCollateral()`, or `getIsolatedFreeCollateral(marketIndex)`
  when a `perpMarketIndex` is passed, returning `ZERO` instead of throwing when that market has no
  isolated bucket.

Call `getMarginCalculation` directly when you need more than one of those values, so the positions
are walked once. New consumers also reach isolated breakdowns through
`isolatedMarginCalculations`.

The margin classes are not re-exported from `src/index.ts`. Package consumers reach them as the
return type of `user.getMarginCalculation()`, or by importing `./marginCalculation` directly.

## Tests

`tests/user/getMarginCalculation.ts` and `tests/user/marginCalculations.test.ts` drive the snapshot
over mock markets and user accounts, covering cross and isolated buckets, open orders, pool-id
rejection and the isolated-tier flags. `tests/sdkParity/marginCategoryFill.test.ts` pins the `'Fill'`
category against `PerpMarket::get_margin_ratio` and `get_unrealized_asset_weight`. Run them with
`bun run test:parity` from `packages/sdk`.
