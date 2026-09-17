import { logger } from '../utils/logger';

/**
 * Node 24 defaults to `--unhandled-rejections=throw`, so one un-awaited promise
 * anywhere kills the process. A rejected Redis write during a reconnect should
 * degrade the publisher rather than stop it, so the handler logs the full
 * context and keeps running. The health check judges liveness, not the rejection
 * count.
 *
 * `uncaughtException` keeps its default handling. A synchronous throw that
 * escapes to the top level leaves unknown state, so exiting is correct there.
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
