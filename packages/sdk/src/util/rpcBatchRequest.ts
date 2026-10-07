/** A single call in a JSON-RPC batch, in the shape `Connection._rpcBatchRequest` accepts. */
export type RpcBatchRequest = { methodName: string; args: any };

/**
 * Sends `requests` as one JSON-RPC batch and returns the responses aligned to the request array,
 * so `responses[i]` always answers `requests[i]`.
 *
 * `Connection._rpcBatchRequest` cannot be used wherever a response has to be tied back to its
 * request. It generates the request ids internally and never exposes them, and it returns the raw
 * response array, which JSON-RPC permits a server to return in any order. Owning the ids is what
 * makes a positional read of the result safe.
 *
 * Everything web3.js layers onto the transport lives in the rpc client's own `callServer`, so it
 * still applies here. That covers rate-limit retries, the keep-alive agent, custom headers and
 * fetch middleware.
 *
 * @param connection Connection whose underlying rpc client sends the batch.
 * @param requests Calls to batch. An empty array resolves to `[]` without contacting the server.
 * @returns One response per request, in request order.
 * @throws (rejects) if the transport fails, if the server answers with anything but a batch array,
 * or if any request goes unanswered.
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
		// The callback takes two parameters. jayson splits errors from results
		// into a third argument when the callback declares one, which returns a
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

	// Ids are matched as strings, because a server may echo `"0"` back for `0`.
	// Matching on the raw value would look like every request in the batch
	// failing, with nothing reporting the real cause.
	const responsesById = new Map<string, any>();
	for (const response of responses) {
		const id = String(response?.id);
		if (responsesById.has(id)) {
			// A Map built through new Map(...) keeps the last duplicate and
			// reports nothing, and every expected id is still present, so a
			// duplicate has to throw here.
			throw new Error(`rpcBatchRequest: duplicate response id ${id}`);
		}
		responsesById.set(id, response);
	}

	return batch.map((request) => {
		const response = responsesById.get(String(request.id));
		if (response === undefined) {
			// This is a correlation failure rather than a server-side one, so it
			// has to throw. Treating it as "no result" drops the request's data
			// and reports nothing.
			throw new Error(
				`rpcBatchRequest: no response matching id ${request.id} for ${request.method} (sent ${batch.length}, received ${responses.length})`
			);
		}
		return response;
	});
}
