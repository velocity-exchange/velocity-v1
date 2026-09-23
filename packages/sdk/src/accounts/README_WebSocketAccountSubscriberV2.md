# WebSocketAccountSubscriberV2

`webSocketAccountSubscriberV2.ts` exports `WebSocketAccountSubscriberV2<T>`, an `AccountSubscriber<T>`
that tracks one account over a WebSocket opened by
[gill](https://www.npmjs.com/package/gill). It keeps the same call shape as
`WebSocketAccountSubscriber` and adds an optional RPC polling fallback for when the socket goes quiet.
`src/index.ts` re-exports the class.

## Usage

```typescript
import { WebSocketAccountSubscriberV2 } from './accounts/webSocketAccountSubscriberV2';

const subscriber = new WebSocketAccountSubscriberV2(
	'userAccount', // account name
	program, // Anchor program instance
	userAccountPublicKey, // PublicKey of the account to subscribe to
	decodeBuffer, // optional custom decode function
	resubOpts, // optional resubscription options
	commitment // optional commitment level
);

// Subscribe to account changes
await subscriber.subscribe((data) => {
	console.log('Account updated:', data);
});

// Unsubscribe when done
await subscriber.unsubscribe();
```

The constructor takes two more optional arguments after `commitment`, `rpcSubscriptions` and `rpc`,
which inject gill clients in place of the ones built from the provider's endpoint. Tests use them.

Omitting `resubOpts` gives you
`{ resubTimeoutMs: 30000, usePollingInsteadOfResub: true, logResubMessages: false }`. A
`resubTimeoutMs` below 1000 logs a warning. `commitment` falls back to the provider's commitment, and
the constructor warns when that is `recent`, `single`, `singleGossip`, `root`, or `max`, none of which
gill accepts.

`unsubscribe()` sets `resubOpts.resubTimeoutMs` to `undefined`, so an instance that has been
unsubscribed will not re-arm its inactivity watchdog if you subscribe it again. The internal
resubscribe path calls `unsubscribe(true)`, which keeps the timeout value.

## Polling instead of resubscribing

For an account that rarely updates, such as a long-tail market, polling avoids tearing down and
rebuilding the subscription every time the watchdog fires:

```typescript
const resubOpts = {
	resubTimeoutMs: 30000, // 30 seconds
	logResubMessages: true,
	usePollingInsteadOfResub: true, // Enable polling mode
	pollingIntervalMs: 30000, // Poll every 30 seconds (optional, defaults to 30000)
};

const subscriber = new WebSocketAccountSubscriberV2(
	'perpMarket', // account name
	program,
	marketPublicKey,
	undefined, // decodeBuffer
	resubOpts
);
```

`subscribe()` seeds state with one `fetch()` when `dataAndSlot` is not already set, opens the
`accountNotifications` subscription, and arms a timeout of `resubTimeoutMs`. Each notification clears
and re-arms that timeout.

When the timeout fires with `receivingData` set, the two branches are:

- `usePollingInsteadOfResub` true: start the polling loop. The WebSocket subscription stays open.
- `usePollingInsteadOfResub` false or absent: call `unsubscribe(true)`, clear `receivingData`, and
  call `subscribe()` again.

The polling loop runs immediately and then every `pollingIntervalMs`, which defaults to 30000. Each
pass records the current buffer, calls `fetch()`, and compares. Unchanged bytes schedule the next
poll. Changed bytes mean the socket dropped an update, so the loop stops and the subscriber
unsubscribes and resubscribes to get a clean stream. A failed poll logs and reschedules rather than
resubscribing. A WebSocket notification arriving while the loop is running stops polling, and the
subscriber carries on from the socket.

## Gill client

```typescript
import { createSolanaClient } from 'gill';

const { rpc, rpcSubscriptions } = createSolanaClient({
	urlOrMoniker: rpcUrl, // or "mainnet", "devnet", etc.
});
```

`rpcSubscriptions.accountNotifications` carries the stream and `rpc.getAccountInfo` serves `fetch()`,
both with `encoding: 'base64'`. Responses from either source go through `handleRpcResponse`, which
decodes gill's `[data, encoding]` tuple into a `Buffer` (base58 through `bs58.decode`, otherwise
base64) and stores it only when there is no cached buffer, or the new slot is not older than the
cached slot and the bytes differ. Accepted updates set `bufferAndSlot` and `dataAndSlot`, then call
`onChange` with the decoded account. Decoding uses `decodeBufferFn` when supplied and the program's
Anchor coder for `accountName` otherwise.

## Differences from the v1 subscriber

- Account reads go through gill's `rpc` instead of a web3.js `Connection`.
- Notifications come from gill's `rpcSubscriptions`, and the `PublicKey` is converted to gill's
  `Address` type at the boundary. `subscribe()` throws `Invalid account public key` when gill's
  `isAddress` rejects it.
- Teardown aborts an `AbortController` passed to `subscribe({ abortSignal })`, which closes the
  socket synchronously, rather than removing a listener id.
- The inactivity timeout can start a polling loop instead of resubscribing.
