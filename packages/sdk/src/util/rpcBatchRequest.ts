/** A single call in a JSON-RPC batch, in the shape `Connection._rpcBatchRequest` accepts. */
export type RpcBatchRequest = { methodName: string; args: any };

/**
 * Sends `requests` as one JSON-RPC batch and returns the responses aligned to the request array,
 * so `responses[i]` always answers `requests[i]`.
 *
 * `Connection._rpcBatchRequest` can't be used wherever a response has to be tied back to its
 * request: it generates the request ids internally and never exposes them, then hands back the raw
 * response array, which JSON-RPC explicitly permits a server to return in any order. Owning the ids
 * is what makes a positional read of the result safe.
 *
 * Everything web3.js layers onto the transport (rate-limit retries, the keep-alive agent, custom
 * headers, fetch middleware) lives in the rpc client's own `callServer`, so it still applies here.
 *
 * @param connection Connection whose underlying rpc client sends the batch.
 * @param requests Calls to batch; an empty array resolves to `[]` without contacting the server.
 * @returns One response per request, in request order.
 * @throws (rejects) if the transport fails, if the server answers with anything but a batch array, or if any request goes unanswered.
 */
export async function rpcBatchRequest(
	connection: any,
	requests: RpcBatchRequest[]
): Promise<any[]> {
	if (requests.length === 0) {
		return [];
	}

	const batch = requests.map((request, index) => ({
		jsonrpc: '2.0',
		id: index,
		method: request.methodName,
		params: request.args,
	}));

	const responses = await new Promise<any>((resolve, reject) => {
		// Two parameters, deliberately: jayson splits errors from results into a
		// third argument if the callback declares one, which would hand back a
		// filtered array rather than the batch.
		connection._rpcClient.request(batch, (error: any, response: any) =>
			error ? reject(error) : resolve(response)
		);
	});

	if (!Array.isArray(responses)) {
		// A batch the server rejects outright comes back as a lone error object,
		// and an empty body comes back as undefined.
		throw new Error(
			`rpcBatchRequest: expected a batch array, got ${JSON.stringify(
				responses
			)}`
		);
	}

	// Ids are matched as strings because echoing `"0"` back for `0` is legal.
	// Missing that would silently look like every request in the batch failing.
	const responsesById = new Map<string, any>();
	for (const response of responses) {
		const id = String(response?.id);
		if (responsesById.has(id)) {
			// A Map built via new Map(...) would silently keep the last one, and
			// every expected id would still be present, so this must be loud too.
			throw new Error(`rpcBatchRequest: duplicate response id ${id}`);
		}
		responsesById.set(id, response);
	}

	return batch.map((request) => {
		const response = responsesById.get(String(request.id));
		if (response === undefined) {
			// Not a server-side failure but a correlation one, so it must be loud:
			// treating it as "no result" would quietly drop the request's data.
			throw new Error(
				`rpcBatchRequest: no response matching id ${request.id} for ${request.method} (sent ${batch.length}, received ${responses.length})`
			);
		}
		return response;
	});
}
