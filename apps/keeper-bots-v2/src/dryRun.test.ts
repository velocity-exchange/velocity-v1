import { expect } from 'chai';
import { Connection } from '@solana/web3.js';
import { Config } from './config';
import { BundleSender } from './bundleSender';
import {
	resolveDryRun,
	applyDryRunFlag,
	refuseBundleSends,
	refuseSends,
} from './dryRun';

function configWith(dryRunByBot: Record<string, boolean>): Config {
	const botConfigs: Record<string, { botId: string; dryRun: boolean }> = {};
	for (const [bot, dryRun] of Object.entries(dryRunByBot)) {
		botConfigs[bot] = { botId: bot, dryRun };
	}

	return {
		enabledBots: Object.keys(dryRunByBot),
		botConfigs,
	} as unknown as Config;
}

describe('dry run', () => {
	it('applies --dry-run to bots configured from a file', () => {
		const config = configWith({ filler: false, liquidator: false });

		applyDryRunFlag(config, true);

		expect(resolveDryRun(config)).to.equal(true);
	});

	it('leaves a live config live', () => {
		const config = configWith({ filler: false });

		applyDryRunFlag(config, false);

		expect(resolveDryRun(config)).to.equal(false);
	});

	it('rejects a config that mixes dry-run and live bots', () => {
		const config = configWith({ filler: true, liquidator: false });

		expect(() => resolveDryRun(config)).to.throw('every enabled bot');
	});

	it('makes every send on a connection throw', async () => {
		const connection = refuseSends(new Connection('http://127.0.0.1:9'));

		for (const send of [
			() => connection.sendRawTransaction(Buffer.alloc(1)),
			() => connection.sendEncodedTransaction('AA=='),
		]) {
			let error: unknown;
			try {
				await send();
			} catch (e) {
				error = e;
			}
			expect(String(error)).to.contain('dry run');
		}
	});

	it('makes a Jito bundle send a no-op', async () => {
		let sent = false;
		const bundleSender = {
			sendTransactions: async () => {
				sent = true;
			},
		} as unknown as BundleSender;

		await refuseBundleSends(bundleSender).sendTransactions([]);

		expect(sent).to.equal(false);
	});
});
