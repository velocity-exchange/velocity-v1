import { logger } from '../utils/logger';

/**
 * Node 24 exits on any unawaited rejection. A Redis write dropped during a
 * reconnect should degrade the publisher, not crash it, so this logs it.
 */
export function installUnhandledRejectionGuard(processName: string): void {
	process.on('unhandledRejection', (reason: unknown) => {
		const detail =
			reason instanceof Error
				? `${reason.message}\n${reason.stack}`
				: String(reason);
		logger.error(`[${processName}] Unhandled promise rejection: ${detail}`);
	});
}
