import { PublicKey } from '@solana/web3.js';
import {
	QUOTE_PRECISION,
	QUOTE_PRECISION_EXP,
	EIGHT,
	LAMPORTS_EXP,
	LAMPORTS_PRECISION,
	SIX,
} from './numericConstants';
import { OracleSource } from '../types';
import { BN } from '../isomorphic/anchor';
import { VelocityEnv } from '../config';

/** Static off-chain metadata for one deployed spot market, keyed by `marketIndex`. Client-side registry (symbol/oracle/mint/decimals wiring) — not decoded from chain, so it must be kept in sync with deployments. */
export type SpotMarketConfig = {
	symbol: string;
	marketIndex: number;
	/** the LP-pool "pool" this market belongs to (distinct spot markets can wrap the same mint under different pool ids, e.g. `SOL` vs `SOL-2`) */
	poolId: number;
	oracle: PublicKey;
	mint: PublicKey;
	oracleSource: OracleSource;
	/** `10^precisionExp`; the token mint's precision as a `BN`, i.e. one whole token */
	precision: BN;
	/** the token mint's decimals */
	precisionExp: BN;
	/** unix ms timestamp the market launched */
	launchTs?: number;
	/** Pyth price-feed id (hex), for the legacy Pyth push/pull oracle path */
	pythFeedId?: string;
	/** Pyth Lazer feed id, for `OracleSource.PYTH_LAZER*` markets */
	pythLazerId?: number;
};

/** The canonical wrapped-SOL mint address, shared across all `SOL`-symbol spot market configs. */
export const WRAPPED_SOL_MINT = new PublicKey(
	'So11111111111111111111111111111111111111112'
);

/**
 * Reflects what is actually deployed on devnet (per on-chain enumeration of
 * `StateAccount.numberOfSpotMarkets`). Update when devnet adds/changes a spot market.
 */
export const DevnetSpotMarkets: SpotMarketConfig[] = [
	{
		symbol: 'dUSDT',
		marketIndex: 0,
		poolId: 0,
		oracle: new PublicKey('Dai8hT1YRBBm5rBSJUSKcdR11psM55LVAkshbypfC4k4'),
		oracleSource: OracleSource.PYTH_LAZER_STABLE_COIN,
		mint: new PublicKey('GqmEqYsy8EyvofDpmtFxK8zhYrgWgNokAtYoduQdL7v6'),
		precision: new BN(10).pow(SIX),
		precisionExp: SIX,
		pythLazerId: 8,
	},
	{
		symbol: 'SOL',
		marketIndex: 1,
		poolId: 0,
		oracle: new PublicKey('2k3UHX6ehRFzx5fTVvbL6FwXhMjkucjJDL9MuVKLo8TV'),
		oracleSource: OracleSource.PYTH_LAZER,
		mint: WRAPPED_SOL_MINT,
		precision: LAMPORTS_PRECISION,
		precisionExp: LAMPORTS_EXP,
		pythLazerId: 6,
	},
];

// Relaunch set from deploy-scripts/params/relaunch-spot-markets.json (PR #188).
// Oracles are the velocity program's pyth_lazer PDAs:
// findProgramAddress(["pyth_lazer", u32le(lazerFeedId)], VELOCITY_PROGRAM_ID).
export const MainnetSpotMarkets: SpotMarketConfig[] = [
	{
		symbol: 'USDT',
		marketIndex: 0,
		poolId: 0,
		oracle: new PublicKey('Dai8hT1YRBBm5rBSJUSKcdR11psM55LVAkshbypfC4k4'),
		oracleSource: OracleSource.PYTH_LAZER_STABLE_COIN,
		mint: new PublicKey('Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB'),
		precision: QUOTE_PRECISION,
		precisionExp: QUOTE_PRECISION_EXP,
		pythFeedId:
			'0x2b89b9dc8fdf9f34709a5b106b472f0f39bb6ca9ce04b0fd7f2e971688e2e53b',
		pythLazerId: 8,
	},
	{
		symbol: 'SOL',
		marketIndex: 1,
		poolId: 0,
		oracle: new PublicKey('2k3UHX6ehRFzx5fTVvbL6FwXhMjkucjJDL9MuVKLo8TV'),
		oracleSource: OracleSource.PYTH_LAZER,
		mint: WRAPPED_SOL_MINT,
		precision: LAMPORTS_PRECISION,
		precisionExp: LAMPORTS_EXP,
		pythFeedId:
			'0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d',
		pythLazerId: 6,
	},
	{
		symbol: 'wBTC',
		marketIndex: 2,
		poolId: 0,
		// Same Pyth Lazer PDA the BTC perp market uses (feed id 1).
		oracle: new PublicKey('J7Fp8iTKuKdCM7PnzqHXTdTo5Jr7ykAmiDyvpz58GJGZ'),
		oracleSource: OracleSource.PYTH_LAZER,
		mint: new PublicKey('3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh'),
		precision: new BN(10).pow(EIGHT),
		precisionExp: EIGHT,
		pythFeedId:
			'0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43',
		pythLazerId: 1,
	},
	{
		symbol: 'wETH',
		marketIndex: 3,
		poolId: 0,
		// Same Pyth Lazer PDA the ETH perp market uses (feed id 2).
		oracle: new PublicKey('6wQ5RiQ7usJ3TqYZSiuAPZHrVCeSWNrvace84khDKEmH'),
		oracleSource: OracleSource.PYTH_LAZER,
		mint: new PublicKey('7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs'),
		precision: new BN(10).pow(EIGHT),
		precisionExp: EIGHT,
		pythFeedId:
			'0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace',
		pythLazerId: 2,
	},
	{
		symbol: 'ZEC',
		marketIndex: 4,
		poolId: 0,
		oracle: new PublicKey('AqpaPcu6PYHrYNySrVptnQnwxVCNxWVFuCgCsr8R1eLQ'),
		oracleSource: OracleSource.PYTH_LAZER,
		mint: new PublicKey('A7bdiYdS5GjqGFtxf17ppRHtDKPkkRqbKtR27dxvQXaS'),
		precision: new BN(10).pow(EIGHT),
		precisionExp: EIGHT,
		pythFeedId:
			'0xbe9b59d178f0d6a97ab4c343bff2aa69caa1eaae3e9048a65788c529b125bb24',
		pythLazerId: 66,
	},
];

/** Spot market registries keyed by `VelocityEnv`, for looking up a deployment's markets without hardcoding the environment. */
export const SpotMarkets: { [key in VelocityEnv]: SpotMarketConfig[] } = {
	devnet: DevnetSpotMarkets,
	'mainnet-beta': MainnetSpotMarkets,
};
