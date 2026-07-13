# WebSocketProgramAccountsSubscriberV2

`WebSocketProgramAccountsSubscriberV2<T>` subscribes to every program account that matches a set of filters. It uses Gill for the WebSocket stream and adds targeted RPC polling for selected accounts so it can detect missed WebSocket updates.

The class implements the same `ProgramAccountSubscriber<T>` interface as `WebSocketProgramAccountSubscriber<T>`, but its runtime behavior is not identical. V2 initially fetches monitored accounts, defaults to a 30-second inactivity check, and can resubscribe when polling finds a missed update.

## When to use it

Use V2 when you need both:

- a WebSocket stream for all matching program accounts; and
- additional missed-update protection for a known set of important accounts.

Accounts that are not in the monitored set still receive normal WebSocket updates. They do not incur the extra RPC polling.

## Usage

The class is exported from the package root:

```typescript
import {
	type PerpMarketAccount,
	WebSocketProgramAccountsSubscriberV2,
} from '@velocity-exchange/sdk';

const monitoredMarketPubkeys = [longTailMarket1, longTailMarket2];

const subscriber =
	new WebSocketProgramAccountsSubscriberV2<PerpMarketAccount>(
		'PerpMarketAccountsSubscriber',
		'perpMarket',
		program,
		(accountName, buffer) =>
			program.coder.accounts.decode(
				accountName,
				buffer
			) as PerpMarketAccount,
		{
			filters: [],
			commitment: 'confirmed',
		},
		undefined, // Use V2's default resubscription options.
		monitoredMarketPubkeys
	);

subscriber.setPollingInterval(60_000);

await subscriber.subscribe((accountId, data, context, buffer) => {
	console.log(accountId.toBase58(), context.slot, data, buffer.length);
});

subscriber.addAccountToMonitor(newMarketPublicKey);
subscriber.removeAccountFromMonitor(oldMarketPublicKey);

await subscriber.unsubscribe();
```

Passing monitored accounts to the constructor causes `subscribe()` to fetch and emit their current state before starting the WebSocket stream. Adding an account with `addAccountToMonitor()` after subscription starts its inactivity timer, but does not fetch it immediately.

## Constructor

```typescript
new WebSocketProgramAccountsSubscriberV2<T>(
	subscriptionName,
	accountDiscriminator,
	program,
	decodeBufferFn,
	options,
	resubOpts,
	accountsToMonitor
);
```

| Parameter | Description |
| --- | --- |
| `subscriptionName` | Human-readable name used in logs. |
| `accountDiscriminator` | Account type name passed to `decodeBufferFn`. |
| `program` | Anchor program whose owned accounts are subscribed to. Its provider RPC endpoint is also used to create the Gill client. |
| `decodeBufferFn` | Converts an account type name and raw `Buffer` into `T`. |
| `options` | Program-account `filters` and optional WebSocket/RPC `commitment`. Defaults to `{ filters: [] }`. |
| `resubOpts` | Inactivity and logging options. If omitted, V2 uses a 30-second inactivity timeout, polling-based missed-update checks, and disabled verbose logs. |
| `accountsToMonitor` | Optional initial `PublicKey[]` whose accounts receive initial fetching and polling safeguards. |

`resubTimeoutMs` values below 1,000 ms log a warning but are still accepted.

## Public methods

### `subscribe(onChange): Promise<void>`

Subscribes once. Calling it again while already subscribed, or while unsubscribing, is a no-op.

For constructor-supplied monitored accounts, `subscribe()` first fetches their current data with `getMultipleAccounts` and invokes `onChange` for new or changed buffers. It then starts the filtered Gill program-account WebSocket stream and the monitoring timers.

The callback receives:

```typescript
(
	accountId: PublicKey,
	data: T,
	context: Context,
	buffer: Buffer
) => void
```

The callback runs only when an account has no cached value or its buffer changes at the same or a newer slot. Older updates and unchanged buffers are ignored.

### `unsubscribe(): Promise<void>`

Clears the inactivity, polling, and pending resubscription timers, then aborts the Gill WebSocket subscription. A manual unsubscribe also clears `resubOpts.resubTimeoutMs` on this instance, so a later manual `subscribe()` does not restore the inactivity timeout automatically.

### `addAccountToMonitor(accountId): void`

Adds an account to the monitored set. If already subscribed, V2 starts an inactivity timer for it. This method does not perform an immediate fetch.

### `removeAccountFromMonitor(accountId): void`

Removes an account from the monitored set and clears its pending and active polling state.

### `setPollingInterval(intervalMs): void`

Changes the monitoring and continuous-polling cadence. The default is 30 seconds. If already subscribed, V2 restarts its per-account monitoring timers with the new interval.

Use this method to change the cadence. The `pollingIntervalMs` field on `ResubOpts` is not read by this class.

## How missed-update detection works

1. `subscribe()` fetches constructor-supplied monitored accounts and seeds the buffer-and-slot cache.
2. The Gill WebSocket stream processes every program account matching `options.filters`.
3. If a monitored account has no recent WebSocket notification for one polling interval, it enters the continuous-polling set.
4. Continuous polling uses `getMultipleAccounts`, splitting active accounts into chunks of at most 100 and fetching those chunks concurrently.
5. A polled buffer at a newer slot that differs from the cached buffer is treated as a missed update.
6. Missed updates detected close together are coalesced for 100 ms, then V2 performs one unsubscribe/subscribe cycle.
7. A changed WebSocket buffer removes that account from the continuous-polling set and returns it to passive monitoring.

If `resubOpts.resubTimeoutMs` is enabled and the entire WebSocket stream is inactive for that duration, V2 also fetches all monitored accounts. It resubscribes only when that fetch reveals a newer, changed buffer.

## Current constraints

- Only monitored accounts receive initial fetching and polling safeguards; all other matching accounts are WebSocket-only.
- Continuous polling chunks requests at 100 accounts. The initial monitored-account fetch and inactivity-timeout check currently send the full monitored set in one `getMultipleAccounts` request, so keep the constructor-supplied monitored set at 100 accounts or fewer.
- A WebSocket notification stops active polling only when its buffer differs from the cached buffer.
- `ResubOpts.pollingIntervalMs` does not configure this class; call `setPollingInterval()` instead.
- `ResubOpts.usePollingInsteadOfResub` is not used as a runtime branch by this class. Its inactivity handler always checks monitored accounts before deciding whether to resubscribe.

## Differences from V1

Both versions implement `ProgramAccountSubscriber<T>` and use the same `subscribe(onChange)` callback shape. V2 additionally:

- uses Gill instead of `Connection.onProgramAccountChange`;
- fetches constructor-supplied monitored accounts before opening the stream;
- enables a 30-second inactivity check when `resubOpts` is omitted;
- polls selected accounts after WebSocket inactivity; and
- coalesces detected missed updates into a single resubscription.

These behavioral differences mean V2 is interface-compatible with V1, but not behaviorally identical.
