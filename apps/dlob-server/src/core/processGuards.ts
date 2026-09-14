import { logger } from '../utils/logger';

/**
 * Node 24 defaults to `--unhandled-rejections=throw`, so one un-awaited promise
 * anywhere kills the process. A rejected Redis write during a reconnect should
 * degrade the publisher, not terminate it, so log with full context and keep
 * running. Liveness is judged by the health check, not by rejection count.
 *
 * `uncaughtException` is deliberately left alone: a synchronous throw that
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
