/**
 * allow-verbose: the usage header of an operator script, and the check a scenario runs.
 *
 * Fill scenarios across the PropAMMs, the book and the vAMM on SOL-PERP:
 *
 *   bun run local:scenario list
 *   bun run local:scenario arrange <name>
 *   bun run local:scenario run <name> [--path onchain|swift]
 *   bun run local:scenario verify <name> (--user <user account> | --authority <wallet>)
 *
 * `arrange` sets the market up for one scenario: it cancels the traders' orders, pauses or resumes
 * the vAMM, quotes each midpoint instance, rests trader c's book asks, and waits until the
 * published L2 shows the arranged depth. It saves that L2 as the snapshot the check reads.
 * `run` arranges, buys as trader b, and checks the fills. `--path onchain` takes on chain, so the
 * order rests behind the speed bump and the relay's taker-origin cross fills it. `--path swift`,
 * the default, signs the order with the market's PropAMM route and sends it through swift.
 * `verify` checks the fills of an order some other client placed after `arrange`, such as the
 * webapp. `--authority` names the wallet, and the check reads its first subaccount.
 *
 * The check decodes every fill of the taker since the snapshot and attributes each leg to the
 * vAMM, the book, or the midpoint instance whose maker it names. It fails when a source fills
 * other than the scenario expects, when a book or PropAMM leg fills at a price the snapshot did
 * not show for that source, or when a leg fills more than the snapshot showed there.
 */
import * as fs from 'fs';
import { execFileSync } from 'child_process';
import {
	Connection,
	ParsedTransactionWithMeta,
	PublicKey,
} from '@solana/web3.js';
import {
	AdminClient,
	BulkAccountLoader,
	getUserAccountPublicKeySync,
	PerpOperation,
	UserClobOrdersClient,
	VELOCITY_PROGRAM_ID,
	Wallet,
} from '@velocity-exchange/sdk';
import { AUTHORITY_PATH, DLOB_URL, loadKey, RPC_URL } from './tools';
import {
	decodeFillLegs,
	FillLeg,
	formatLegs,
	makerLabels,
} from './scenario-fills';

const MARKET = 'SOL-PERP';
const MARKET_INDEX = 0;
const SNAPSHOT_DIR = '/state/scenarios';
const L2_DEPTH = 20;
const ARRANGE_TIMEOUT_MS = 60_000;
const FILL_TIMEOUT_MS = 45_000;
const POLL_MS = 1_000;
/** A midpoint instance a scenario leaves out quotes this far above the oracle, past any worst. */
const PARKED_MID_PCT = 10;
const INSTANCES = ['a', 'b'];
const PRICE_TOLERANCE = 0.0002;

type Anchor = 'oracle' | 'vamm';

/** A price as a percent offset from the oracle or from the vAMM's best ask. */
type RelativePrice = { from: Anchor; pct: number };

type Scenario = {
	summary: string;
	vamm: 'live' | 'paused';
	/** Each listed midpoint instance quotes around this mid. Its rungs sit 10 and 30 bps above it. */
	propamms: Record<string, RelativePrice>;
	bookAsks: { price: RelativePrice; size: string }[];
	buy: { size: string; worst: RelativePrice };
	/** Base each source fills. `'any'` accepts a nonzero amount that the curve decides. */
	expect: Record<string, number | 'any'>;
	/** Base the order leaves resting on the book. */
	rests: number;
};

const SCENARIOS: Record<string, Scenario> = {
	'rung-walk': {
		summary:
			'One PropAMM fills a buy across both of its ask rungs. The vAMM quotes far above.',
		vamm: 'live',
		propamms: { a: { from: 'oracle', pct: 0 } },
		bookAsks: [],
		buy: { size: '1.5', worst: { from: 'oracle', pct: 1 } },
		expect: { 'propamm:a': 1.5 },
		rests: 0,
	},
	'partial-rest': {
		summary:
			'One PropAMM fills what it quotes, and the remainder rests on the book. The vAMM is paused.',
		vamm: 'paused',
		propamms: { a: { from: 'oracle', pct: 0 } },
		bookAsks: [],
		buy: { size: '2.5', worst: { from: 'oracle', pct: 0.5 } },
		expect: { 'propamm:a': 2 },
		rests: 0.5,
	},
	'three-way': {
		summary:
			'A buy splits across a PropAMM, a book ask and the vAMM, at the best price first.',
		vamm: 'live',
		propamms: { a: { from: 'vamm', pct: -0.2 } },
		bookAsks: [{ price: { from: 'vamm', pct: -0.05 }, size: '0.5' }],
		buy: { size: '3', worst: { from: 'vamm', pct: 0.5 } },
		expect: { 'propamm:a': 'any', clob: 0.5, vamm: 'any' },
		rests: 0,
	},
	'two-propamms': {
		summary:
			'A buy walks two PropAMMs best price first. The second quotes 20 bps above the first.',
		vamm: 'live',
		propamms: {
			a: { from: 'oracle', pct: 0 },
			b: { from: 'oracle', pct: 0.2 },
		},
		bookAsks: [],
		buy: { size: '2.5', worst: { from: 'oracle', pct: 1 } },
		expect: { 'propamm:a': 2, 'propamm:b': 0.5 },
		rests: 0,
	},
	'propamm-tie': {
		summary:
			'Two PropAMMs quote the same rungs at one priority, so each rung fills them pro rata.',
		vamm: 'live',
		propamms: {
			a: { from: 'oracle', pct: 0 },
			b: { from: 'oracle', pct: 0 },
		},
		bookAsks: [],
		buy: { size: '2.5', worst: { from: 'oracle', pct: 1 } },
		expect: { 'propamm:a': 1.25, 'propamm:b': 1.25 },
		rests: 0,
	},
};

type L2Level = { price: string; size: string; sources: Record<string, string> };
type L2 = { asks: L2Level[]; oracle: number; slot: number };

type Snapshot = {
	scenario: string;
	slot: number;
	worst: number;
	asks: { price: number; sources: Record<string, number> }[];
};

const usd = (raw: string | number) => Number(raw) / 1e6;
const sol = (raw: string | number) => Number(raw) / 1e9;

async function fetchL2(): Promise<L2> {
	const url = `${DLOB_URL}/l2?marketIndex=${MARKET_INDEX}&marketType=perp&depth=${L2_DEPTH}&includeVamm=true`;
	return (await fetch(url)).json() as Promise<L2>;
}

function vammBestAsk(l2: L2): number {
	const level = l2.asks.find((ask) => ask.sources.vamm);
	if (!level) throw new Error('the L2 shows no vAMM ask');
	return usd(level.price);
}

function resolvePrice(price: RelativePrice, l2: L2): number {
	const anchor = price.from === 'oracle' ? usd(l2.oracle) : vammBestAsk(l2);
	return anchor * (1 + price.pct / 100);
}

/** Fixed-point text for the operator scripts, which refuse more decimals than the market uses. */
const decimal = (value: number) => value.toFixed(4);

function stackScript(script: string, args: string[]): void {
	execFileSync('bun', ['run', `local-stack/${script}.ts`, ...args], {
		stdio: 'inherit',
		timeout: 120_000,
	});
}

async function setVamm(connection: Connection, state: Scenario['vamm']) {
	const admin = new AdminClient({
		connection,
		wallet: new Wallet(loadKey(AUTHORITY_PATH)),
		env: 'devnet',
		perpMarketIndexes: [MARKET_INDEX],
		spotMarketIndexes: [0],
		accountSubscription: {
			type: 'polling',
			accountLoader: new BulkAccountLoader(connection, 'confirmed', 1000),
		},
	});
	await admin.subscribe();

	try {
		const paused = state === 'paused' ? PerpOperation.AMM_FILL : 0;
		await admin.updatePerpMarketPausedOperations(MARKET_INDEX, paused);
		console.log(`vAMM ${state}`);
	} finally {
		await admin.unsubscribe();
	}
}

function quotePropamms(scenario: Scenario, l2: L2): void {
	for (const instance of INSTANCES) {
		const mid = scenario.propamms[instance]
			? resolvePrice(scenario.propamms[instance], l2)
			: usd(l2.oracle) * (1 + PARKED_MID_PCT / 100);
		stackScript('midpoint', ['up', decimal(mid), '--name', instance]);
	}
}

/** Waits until the L2 shows vAMM depth when the vAMM is live and none when it is paused. */
async function awaitVammState(state: Scenario['vamm']): Promise<L2> {
	const deadline = Date.now() + ARRANGE_TIMEOUT_MS;
	while (Date.now() < deadline) {
		const l2 = await fetchL2();
		const shown = l2.asks.some((ask) => ask.sources.vamm);
		if (shown === (state === 'live')) return l2;

		await new Promise((resolve) => setTimeout(resolve, POLL_MS));
	}

	throw new Error(`the L2 never showed the vAMM ${state}`);
}

/** Waits until the L2 shows every source the scenario arranged, then returns it. */
async function awaitArrangedL2(scenario: Scenario, afterSlot: number) {
	const wanted = new Set(
		Object.keys(scenario.expect).map((source) => source.split(':')[0])
	);
	const deadline = Date.now() + ARRANGE_TIMEOUT_MS;

	while (Date.now() < deadline) {
		const l2 = await fetchL2();
		const shown = new Set(l2.asks.flatMap((ask) => Object.keys(ask.sources)));
		if (l2.slot > afterSlot && [...wanted].every((s) => shown.has(s))) {
			return l2;
		}

		await new Promise((resolve) => setTimeout(resolve, POLL_MS));
	}

	throw new Error(`the L2 never showed ${[...wanted].join(', ')}`);
}

async function arrange(connection: Connection, name: string) {
	const scenario = scenarioNamed(name);
	console.log(`arranging ${name}: ${scenario.summary}`);

	stackScript('trader', ['--name', 'b', 'cancel', MARKET]);
	stackScript('trader', ['--name', 'c', 'cancel', MARKET]);
	await setVamm(connection, scenario.vamm);

	const before = await awaitVammState(scenario.vamm);
	quotePropamms(scenario, before);
	for (const ask of scenario.bookAsks) {
		const price = decimal(resolvePrice(ask.price, before));
		stackScript('trader', [
			'--name',
			'c',
			'rest',
			MARKET,
			'ask',
			price,
			ask.size,
		]);
	}

	const l2 = await awaitArrangedL2(scenario, await connection.getSlot());
	const snapshot: Snapshot = {
		scenario: name,
		slot: l2.slot,
		worst: resolvePrice(scenario.buy.worst, before),
		asks: l2.asks.map((ask) => ({
			price: usd(ask.price),
			sources: Object.fromEntries(
				Object.entries(ask.sources).map(([source, size]) => [source, sol(size)])
			),
		})),
	};

	fs.mkdirSync(SNAPSHOT_DIR, { recursive: true });
	fs.writeFileSync(`${SNAPSHOT_DIR}/${name}.json`, JSON.stringify(snapshot));
	printSnapshot(snapshot);
	return snapshot;
}

function printSnapshot(snapshot: Snapshot): void {
	console.log(
		`L2 asks at slot ${snapshot.slot} (buy worst ${decimal(snapshot.worst)}):`
	);
	for (const ask of snapshot.asks.filter((a) => a.price <= snapshot.worst)) {
		const sources = Object.entries(ask.sources)
			.map(([source, size]) => `${source} ${size}`)
			.join(', ');
		console.log(`  ${decimal(ask.price)}  ${sources}`);
	}
}

function scenarioNamed(name: string): Scenario {
	const scenario = SCENARIOS[name];
	if (!scenario) {
		throw new Error(
			`unknown scenario "${name}". Scenarios: ${Object.keys(SCENARIOS).join(
				', '
			)}`
		);
	}

	return scenario;
}

async function main() {
	const argv = process.argv.slice(2);
	const flag = (name: string) => {
		const at = argv.indexOf(`--${name}`);
		return at === -1 ? undefined : argv.splice(at, 2)[1];
	};
	const path = flag('path') ?? 'swift';
	const authority = flag('authority');
	const user =
		flag('user') ??
		(authority &&
			getUserAccountPublicKeySync(
				new PublicKey(VELOCITY_PROGRAM_ID),
				new PublicKey(authority),
				0
			).toBase58());
	const [command, name] = argv;
	const connection = new Connection(RPC_URL, 'confirmed');

	if (command === 'list') {
		for (const [key, scenario] of Object.entries(SCENARIOS)) {
			console.log(`${key.padEnd(14)} ${scenario.summary}`);
		}
	} else if (command === 'arrange') await arrange(connection, name);
	else if (command === 'run') await run(connection, name, path);
	else if (command === 'verify' && user) {
		await verify(connection, name, new PublicKey(user));
	} else {
		throw new Error(
			'commands: list, arrange <name>, run <name> [--path onchain|swift], verify <name> --user <user> | --authority <wallet>'
		);
	}
}

async function run(connection: Connection, name: string, path: string) {
	const scenario = scenarioNamed(name);
	const snapshot = await arrange(connection, name);
	const worst = decimal(snapshot.worst);

	if (path === 'onchain') {
		stackScript('trader', [
			'--name',
			'b',
			'take',
			MARKET,
			'buy',
			scenario.buy.size,
			'--worst',
			worst,
		]);
	} else if (path === 'swift') {
		stackScript('trader', [
			'--name',
			'b',
			'swift',
			MARKET,
			'buy',
			scenario.buy.size,
			'--worst',
			worst,
		]);
	} else {
		throw new Error(`--path is onchain or swift, not "${path}"`);
	}

	const trader = loadKey('/state/keys/trader-b.json').publicKey;
	const user = getUserAccountPublicKeySync(
		new PublicKey(VELOCITY_PROGRAM_ID),
		trader,
		0
	);
	await verify(connection, name, user);
}

/** Every fill leg the taker received since the snapshot, waiting until the expected base fills. */
async function awaitLegs(
	connection: Connection,
	user: PublicKey,
	snapshot: Snapshot,
	expectedBase: number
): Promise<FillLeg[]> {
	const deadline = Date.now() + FILL_TIMEOUT_MS;
	const labels = await makerLabels(connection, MARKET_INDEX, INSTANCES);
	let legs: FillLeg[] = [];

	while (Date.now() < deadline) {
		const signatures = await connection.getSignaturesForAddress(user, {
			limit: 25,
		});
		const recent = signatures.filter((s) => s.slot >= snapshot.slot && !s.err);
		const transactions: (ParsedTransactionWithMeta | null)[] =
			await connection.getParsedTransactions(
				recent.map((s) => s.signature),
				{ maxSupportedTransactionVersion: 1 }
			);
		legs = decodeFillLegs(transactions, user, labels);

		const filled = legs.reduce((total, leg) => total + leg.base, 0);
		if (filled >= expectedBase - 1e-9) return legs;

		await new Promise((resolve) => setTimeout(resolve, POLL_MS));
	}

	return legs;
}

async function verify(connection: Connection, name: string, user: PublicKey) {
	const scenario = scenarioNamed(name);
	const snapshot = JSON.parse(
		fs.readFileSync(`${SNAPSHOT_DIR}/${name}.json`, 'utf-8')
	) as Snapshot;

	const expectedBase = Number(scenario.buy.size) - scenario.rests;
	const legs = await awaitLegs(connection, user, snapshot, expectedBase);
	console.log(formatLegs(legs));

	const failures = [
		...checkSources(scenario, legs),
		...checkAgainstSnapshot(snapshot, legs),
		...(await checkRemainder(user, scenario.rests)),
	];
	if (failures.length) {
		failures.forEach((failure) => console.error(`FAIL ${failure}`));
		process.exit(1);
	}

	console.log(`PASS ${name}`);
}

function checkSources(scenario: Scenario, legs: FillLeg[]): string[] {
	const filled = new Map<string, number>();
	legs.forEach((leg) =>
		filled.set(leg.source, (filled.get(leg.source) ?? 0) + leg.base)
	);

	const unexpected = [...filled.keys()]
		.filter((source) => !(source in scenario.expect))
		.map(
			(source) =>
				`${source} filled ${filled.get(source)} and the scenario expects none`
		);
	const wrong = Object.entries(scenario.expect).flatMap(([source, base]) => {
		const got = filled.get(source) ?? 0;
		if (base === 'any') return got > 0 ? [] : [`${source} filled nothing`];
		return Math.abs(got - base) < 1e-6
			? []
			: [`${source} filled ${got}, expected ${base}`];
	});

	return [...unexpected, ...wrong];
}

type Level = { price: number; size: number };

type Walk = {
	cost: number;
	/** The price of the last level the walk took from. */
	worstTaken: number;
	/** The price of the first level the walk left depth at. */
	nextLeft: number;
};

/** Buys `base` from `levels`, cheapest first. `undefined` when they hold less. */
function walk(levels: Level[], base: number): Walk | undefined {
	let remaining = base;
	let cost = 0;
	let worstTaken = 0;
	let nextLeft = Infinity;
	for (const level of levels) {
		const take = Math.min(remaining, level.size);
		if (take > 1e-9) worstTaken = level.price;
		if (level.size - take > 1e-9 && nextLeft === Infinity) {
			nextLeft = level.price;
		}

		cost += take * level.price;
		remaining -= take;
	}

	return remaining > 1e-9 ? undefined : { cost, worstTaken, nextLeft };
}

function sourceLevels(snapshot: Snapshot, source: string): Level[] {
	return snapshot.asks
		.filter((ask) => ask.sources[source])
		.map((ask) => ({ price: ask.price, size: ask.sources[source] }));
}

/** Base and cost per source. The L2 merges every PropAMM under one source, so the legs are summed. */
function filledBySource(
	legs: FillLeg[]
): Map<string, { base: number; cost: number }> {
	const filled = new Map<string, { base: number; cost: number }>();
	for (const leg of legs) {
		const source = leg.source.split(':')[0];
		const total = filled.get(source) ?? { base: 0, cost: 0 };
		filled.set(source, {
			base: total.base + leg.base,
			cost: total.cost + leg.base * leg.price,
		});
	}

	return filled;
}

/**
 * Each book and PropAMM source must fill at the cost of walking the depth the snapshot showed for
 * it, cheapest first. No source may then take a price above one another source left depth at. A
 * vAMM leg is an average along the curve, so it is held to the vAMM's best ask, and no other
 * source may leave depth below that average.
 */
function checkAgainstSnapshot(snapshot: Snapshot, legs: FillLeg[]): string[] {
	const filled = filledBySource(legs);
	const failures: string[] = [];
	const walks = new Map<string, Walk>();

	for (const source of ['clob', 'propamm']) {
		const { base, cost } = filled.get(source) ?? { base: 0, cost: 0 };
		const walked = walk(sourceLevels(snapshot, source), base);
		if (!walked) {
			failures.push(`${source} filled ${base}, more than the L2 showed`);
			continue;
		}

		walks.set(source, walked);
		if (base > 0 && Math.abs(walked.cost - cost) / base > PRICE_TOLERANCE) {
			failures.push(
				`${source} filled ${base} at ${decimal(
					cost / base
				)}, and its L2 depth prices that at ${decimal(walked.cost / base)}`
			);
		}
	}

	const vamm = filled.get('vamm');
	const marginals = [...walks.entries()]
		.filter(([source]) => filled.has(source))
		.map(([source, walked]) => ({ source, price: walked.worstTaken }));
	if (vamm) {
		const average = vamm.cost / vamm.base;
		const best = sourceLevels(snapshot, 'vamm')[0]?.price ?? Infinity;
		if (average < best - PRICE_TOLERANCE) {
			failures.push(
				`vamm filled at ${decimal(average)}, under its best ask ${decimal(
					best
				)}`
			);
		}

		marginals.push({ source: 'vamm', price: average });
	}

	for (const [source, walked] of walks) {
		for (const marginal of marginals) {
			if (
				marginal.source !== source &&
				marginal.price > walked.nextLeft + PRICE_TOLERANCE
			) {
				failures.push(
					`${marginal.source} filled at ${decimal(
						marginal.price
					)} while ${source} left depth at ${decimal(walked.nextLeft)}`
				);
			}
		}
	}

	return failures;
}

/** Polls the user-orders feed, which trails the chain by a publish, for the taker's remainder. */
async function checkRemainder(
	user: PublicKey,
	rests: number
): Promise<string[]> {
	const feed = new UserClobOrdersClient(DLOB_URL);
	const deadline = Date.now() + ARRANGE_TIMEOUT_MS;
	let resting = 0;

	while (Date.now() < deadline) {
		const rows = await feed.fetch(user, [MARKET_INDEX]);
		resting = rows
			.filter((row) => row.takerOrigin)
			.reduce((total, row) => total + sol(row.baseAssetAmount.toString()), 0);
		if (Math.abs(resting - rests) < 1e-6) return [];

		await new Promise((resolve) => setTimeout(resolve, POLL_MS));
	}

	return [`${resting} rests as a taker remainder, expected ${rests}`];
}

main()
	.then(() => process.exit(0))
	.catch((error) => {
		console.error(error instanceof Error ? error.message : error);
		process.exit(1);
	});
