# WebSocketProgramAccountsSubscriberV2

`webSocketProgramAccountSubscriberV2.ts` exports `WebSocketProgramAccountsSubscriberV2<T>`, a
`ProgramAccountSubscriber<T>` that streams program accounts over a WebSocket opened by
[gill](https://www.npmjs.com/package/gill) and adds RPC polling for a named subset of accounts so a
stalled socket does not silently drop their updates.

A near-identical copy of the class lives in `webSocketProgramAccountsSubscriberV2.ts` (plural
`Accounts` in the filename). That copy is the one `src/index.ts` re-exports and the one
`webSocketVelocityClientAccountSubscriberV2.ts` instantiates for the perp-market and spot-market
streams. Two behavioral differences separate them. In this file, `removeAccountFromMonitor` also
clears the account's `lastWsNotificationTime` and missed-update entries, and dropping an account from
the polling set on a WebSocket notification requires a decoded buffer.

## Usage

```typescript
import { WebSocketProgramAccountsSubscriberV2 } from './accounts/webSocketProgramAccountSubscriberV2';

const subscriber = new WebSocketProgramAccountsSubscriberV2(
	'perpMarket', // subscriptionName, used in logs
	'perpMarket', // accountDiscriminator, the Anchor account type name
	program, // Anchor program instance
	decodeBuffer, // (accountName, buffer) => T
	{ filters: [] }, // memcmp filters and commitment
	resubOpts, // optional resubscription options
	[longTailMarket1, longTailMarket2] // optional accounts to monitor
);

await subscriber.subscribe((accountId, data, context, buffer) => {
	console.log('Account updated:', accountId.toBase58(), data);
});

// Add or drop monitored accounts after subscribing.
subscriber.addAccountToMonitor(newMarketPublicKey);
subscriber.removeAccountFromMonitor(oldMarketPublicKey);

// Change how often monitored accounts are checked and polled.
subscriber.setPollingInterval(60000); // 60 seconds

await subscriber.unsubscribe();
```

`unsubscribe()` sets `resubOpts.resubTimeoutMs` to `undefined`, so an instance that has been
unsubscribed will not re-arm its inactivity watchdog if you subscribe it again. The internal
resubscribe path calls `unsubscribe(true)`, which keeps the timeout value.

## Constructor parameters

- `subscriptionName`: name used in log lines.
- `accountDiscriminator`: Anchor account type name, passed to `decodeBufferFn` for every update.
- `program`: Anchor program. Its `programId` is the subscription target and its provider's
  `connection.rpcEndpoint` is the URL passed to gill's `createSolanaClient`.
- `decodeBufferFn`: `(accountName: string, buffer: Buffer) => T`.
- `options`: `{ filters: MemcmpFilter[]; commitment?: Commitment }`, defaulting to `{ filters: [] }`.
- `resubOpts`: resubscription and polling options. When omitted it defaults to
  `{ resubTimeoutMs: 30000, usePollingInsteadOfResub: true, logResubMessages: false }`. The v1
  subscriber has no default, so omitting `resubOpts` there disables the watchdog entirely. This class
  never reads `usePollingInsteadOfResub`; monitored-account polling is always on. A
  `resubTimeoutMs` below 1000 logs a warning.
- `accountsToMonitor`: optional `PublicKey[]` seeding the monitored set. `addAccountToMonitor` and
  `removeAccountFromMonitor` change it later.

Polling cadence is not a constructor parameter. It starts at 30000 ms and `setPollingInterval(ms)`
changes it, restarting the per-account monitoring timers if the subscription is live.

## Gill client

```typescript
import { createSolanaClient } from 'gill';

const { rpc, rpcSubscriptions } = createSolanaClient({
	urlOrMoniker: rpcUrl, // or "mainnet", "devnet", etc.
});
```

`rpcSubscriptions.programNotifications` carries the stream, `rpc.getMultipleAccounts` serves every
poll, and both use `encoding: 'base64'`. Filter bytes that look like base58 are decoded and re-encoded
as base64 before they go on the wire, because gill's memcmp filter takes a `bigint` offset and
base64 bytes.

## Subscribing and streaming

`subscribe(onChange)` returns early if a subscription already exists or an unsubscribe is in flight.
Otherwise it runs a single `getMultipleAccounts` over every monitored account, stores the results in
`bufferAndSlotMap`, and emits them through `onChange` so consumers start from current state instead
of waiting for the first notification. That seeding pass does not test for missed updates. The class
then opens the `programNotifications` subscription, arms the inactivity watchdog if
`resubOpts.resubTimeoutMs` is set, and starts the per-account monitoring timers.

Every notification and every polled response goes through `handleRpcResponse`, which decodes gill's
`[data, encoding]` tuple into a `Buffer` (base58 through `bs58.decode`, otherwise base64) and stores
it only when there is no cached entry for that account, or the new slot is not older than the cached
slot and the bytes differ. Accepted updates call `onChange(accountId, data, { slot }, buffer)`.

## Polling monitored accounts

Only accounts in the monitored set get polling. Everything else the filters match still flows through
the WebSocket as usual.

1. Each monitored account gets a timer of `pollingIntervalMs`. When it fires, the class compares
   `Date.now()` against the last WebSocket notification recorded for that account. If a notification
   arrived within the interval, the timer re-arms. If not, the account joins
   `accountsCurrentlyPolling`.
2. Adding an account to the polling set schedules a poll 100 ms out. The delay batches accounts that
   go quiet at the same time into one request.
3. That poll calls `getMultipleAccounts` for the whole polling set, chunked at 100 addresses per
   request with the chunks sent concurrently through `Promise.all`. Batch polls then repeat every
   `pollingIntervalMs` for as long as the polling set is non-empty.
4. A polled account whose response is at a later slot with different bytes than the cached buffer
   counts as a missed update.
5. A WebSocket notification removes an account from the polling set when its buffer differs from the
   cached one. Once the set empties, the batch timer is cleared.

## Detecting missed updates and resubscribing

`signalMissedChange` records the account, sets the missed-change flag, and schedules the
resubscription 100 ms later. Accounts that report a missed update inside that window join the same
batch rather than triggering their own resubscribe. The handler then calls `unsubscribe(true)`,
clears `receivingData`, and calls `subscribe(onChange)` again, which replays the seeding fetch and
opens a fresh socket.

The inactivity watchdog is the second path into the same flow. Each accepted notification clears and
re-arms it. If `resubTimeoutMs` elapses while `receivingData` is true, the class fetches every
monitored account in one `getMultipleAccounts` call and looks for a cached entry that is behind. A
hit signals a missed change under the identifier `timeout-check`. A miss clears `receivingData` and
re-arms the timer.

## Differences from the v1 subscriber

`WebSocketProgramAccountSubscriber` and this class both implement `ProgramAccountSubscriber<T>`, so a
call site changes the constructor name and optionally passes `accountsToMonitor`. Underneath:

- Account reads go through gill's `rpc` instead of a web3.js `Connection`.
- Notifications come from gill's `rpcSubscriptions`, and `PublicKey` values are converted to gill's
  `Address` type at the boundary.
- Teardown aborts an `AbortController` passed to `subscribe({ abortSignal })`, which closes the
  socket synchronously, rather than removing a listener id.
- Monitored accounts get the polling safeguard described above.

Some gill types do not line up with what the code needs, so encoding comparisons and memcmp filter
bytes are cast through `any`.
