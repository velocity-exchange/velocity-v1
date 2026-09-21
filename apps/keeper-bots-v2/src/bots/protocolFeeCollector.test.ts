import { expect } from 'chai';
import { BN, ZERO } from '@velocity-exchange/sdk';
import { PublicKey } from '@solana/web3.js';
import { getRouterConfigPda } from '@velocity-exchange/revenue-router-sdk';

import { ProtocolFeeCollectorBot } from './protocolFeeCollector';

type SendCall = { label: string };

// One perp market with fees waiting. No pending carveout, so the run skips the
// sweep (and its 15s confirmation wait) and goes straight to the withdrawal.
const PERP_MARKET = {
	marketIndex: 0,
	quoteSpotMarketIndex: 0,
	protocolFeePool: { scaledBalance: new BN(10).pow(new BN(13)) },
	feeLedger: { pendingProtocolFee: ZERO },
};

const QUOTE_SPOT_MARKET = {
	marketIndex: 0,
	decimals: 6,
	cumulativeDepositInterest: new BN(10).pow(new BN(10)),
	protocolFeePool: { scaledBalance: ZERO },
};

function makeBot(opts: {
	withdrawResult: { sent: boolean; confirmedSlot?: number };
	distributeResult?: { sent: boolean; confirmedSlot?: number };
	routerOk?: boolean;
}): { bot: ProtocolFeeCollectorBot; calls: SendCall[] } {
	const bot = Object.create(ProtocolFeeCollectorBot.prototype) as any;
	const calls: SendCall[] = [];

	bot.name = 'test-protocol-fee-collector';
	bot.dryRun = false;
	bot.defaultIntervalMs = 24 * 60 * 60 * 1000;
	bot.watchdogTimerLastPatTime = Date.now();
	bot.watchdogTimerMutex = { runExclusive: (fn: () => any) => fn() };

	bot.adminClient = {
		connection: {},
		wallet: { publicKey: PublicKey.default },
		getStateAccount: () => ({
			protocolFeeRecipientPerp: getRouterConfigPda(),
			protocolFeeRecipientSpot: PublicKey.default,
		}),
		getPerpMarketAccounts: () => [PERP_MARKET],
		getSpotMarketAccounts: () => [QUOTE_SPOT_MARKET],
		getSpotMarketAccountOrThrow: () => QUOTE_SPOT_MARKET,
		getSweepPerpMarketFeesIx: async () => ({}),
		getWithdrawProtocolFeesPerpIx: async () => ({}),
	};

	bot.checkRouterConfig = async () => opts.routerOk ?? true;
	bot.buildDistributeIx = async () => ({});
	bot.sendIx = async (
		_ix: unknown,
		_marketType: string,
		_marketIndex: number,
		label: string
	) => {
		calls.push({ label });
		if (label === 'distribute') {
			return opts.distributeResult ?? { sent: true, confirmedSlot: 1 };
		}
		return opts.withdrawResult;
	};

	return { bot: bot as ProtocolFeeCollectorBot, calls };
}

function labels(calls: SendCall[]): string[] {
	return calls.map((c) => c.label);
}

describe('ProtocolFeeCollectorBot router distribute step', () => {
	it('returns unhealthy while unhealthyReason is set', async () => {
		const { bot } = makeBot({
			withdrawResult: { sent: true, confirmedSlot: 1 },
		});
		expect(await bot.healthCheck()).to.equal(true);

		(bot as any).unhealthyReason = 'distribute failed';
		expect(await bot.healthCheck()).to.equal(false);
	});

	it('skips distribute when a perp withdrawal has no confirmed slot', async () => {
		const { bot, calls } = makeBot({
			withdrawResult: { sent: true, confirmedSlot: undefined },
		});

		await (bot as any).tryCollectProtocolFees();

		expect(labels(calls)).to.include('withdrawProtocolFeesPerp');
		expect(labels(calls)).to.not.include('distribute');
	});

	it('runs distribute after a confirmed withdrawal', async () => {
		const { bot, calls } = makeBot({
			withdrawResult: { sent: true, confirmedSlot: 42 },
		});

		await (bot as any).tryCollectProtocolFees();

		expect(labels(calls)).to.include('distribute');
		expect((bot as any).unhealthyReason).to.equal(undefined);
	});

	it('skips distribute when the router config check fails', async () => {
		const { bot, calls } = makeBot({
			withdrawResult: { sent: true, confirmedSlot: 42 },
			routerOk: false,
		});

		await (bot as any).tryCollectProtocolFees();

		expect(labels(calls)).to.not.include('distribute');
	});

	it('marks the bot unhealthy when distribute fails', async () => {
		const { bot } = makeBot({
			withdrawResult: { sent: true, confirmedSlot: 42 },
			distributeResult: { sent: false },
		});

		await (bot as any).tryCollectProtocolFees();

		expect((bot as any).unhealthyReason).to.equal('distribute failed');
		expect(await bot.healthCheck()).to.equal(false);
	});

	it('marks the bot unhealthy when the distribute ix cannot be built', async () => {
		const { bot, calls } = makeBot({
			withdrawResult: { sent: true, confirmedSlot: 42 },
		});
		(bot as any).buildDistributeIx = async () => {
			throw new Error('RouterConfig is not initialized');
		};

		await (bot as any).tryCollectProtocolFees();

		expect(labels(calls)).to.not.include('distribute');
		expect((bot as any).unhealthyReason).to.equal('distribute failed');
	});
});

describe('revenue-router SDK', () => {
	it('derives a deterministic router config PDA', () => {
		const expected = PublicKey.findProgramAddressSync(
			[Buffer.from('router_config')],
			new PublicKey('39PAxdVaWHYH62bR5AWTkMVd52Y4QeJLjQ8TChupghgT')
		)[0];
		expect(getRouterConfigPda().toBase58()).to.equal(expected.toBase58());
	});
});
