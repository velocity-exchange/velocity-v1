import { Connection } from '@solana/web3.js';
import { BundleSender } from './bundleSender';
import { Config } from './config';
import { logger } from './logger';

/** `--dry-run` overrides every bot, including bots configured from a file. */
export function applyDryRunFlag(config: Config, dryRunFlag: boolean): void {
	if (!dryRunFlag) {
		return;
	}

	for (const botConfig of Object.values(config.botConfigs ?? {})) {
		if (botConfig) {
			botConfig.dryRun = true;
		}
	}
}

/**
 * Returns whether the process runs dry. Dry run is enforced at the connection,
 * so it holds for the whole process. Throws on a config that mixes dry-run and
 * live bots.
 */
export function resolveDryRun(config: Config): boolean {
	const dryRunBots = config.enabledBots.filter(
		(bot) => (config.botConfigs as any)?.[bot]?.dryRun === true
	);
	if (dryRunBots.length > 0 && dryRunBots.length < config.enabledBots.length) {
		throw new Error(
			`dryRun must be set on every enabled bot or none; set on: ${dryRunBots.join(
				', '
			)}`
		);
	}

	return dryRunBots.length > 0;
}

/**
 * Makes every send on `connection` throw. A bot that ignores its own `dryRun`
 * flag, or a helper that builds its own tx sender, still sends through a connection.
 */
export function refuseSends(connection: Connection): Connection {
	const refuse = async (): Promise<never> => {
		throw new Error('dry run: refusing to send a transaction');
	};

	connection.sendRawTransaction = refuse;
	connection.sendTransaction = refuse;
	connection.sendEncodedTransaction = refuse;
	return connection;
}

/**
 * Makes Jito bundle sends no-ops. Callers often do not await a bundle send, so a
 * throw here would surface as an unhandled rejection rather than a skipped send.
 */
export function refuseBundleSends(bundleSender: BundleSender): BundleSender {
	bundleSender.sendTransactions = async () => {
		logger.info('dry run: not sending a Jito bundle');
	};
	return bundleSender;
}
