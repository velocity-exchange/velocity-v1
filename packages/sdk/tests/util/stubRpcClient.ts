import { Connection } from '@solana/web3.js';

/**
 * Builds a `Connection` stand-in whose `_rpcClient` answers a JSON-RPC batch by running `respond`
 * once per request, tagging each response with its request's id.
 *
 * `transform` reshapes the response array before it is handed back, which is how a server that
 * reorders, drops, or re-types responses is simulated. JSON-RPC permits all of those.
 */
export function stubRpcClient(
	respond: (params: any[], method: string) => any,
	transform: (responses: any[]) => any = (responses) => responses
): Connection {
	return {
		_rpcClient: {
			request: (
				batch: Array<{ id: number; method: string; params: any[] }>,
				callback: (error: any, responses: any) => void
			) =>
				callback(
					null,
					transform(
						batch.map((request) => ({
							id: request.id,
							...respond(request.params, request.method),
						}))
					)
				),
		},
	} as unknown as Connection;
}
