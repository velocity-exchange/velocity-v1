# velocity-pubsub-client

Drop-in replacement for the Solana/Agave `PubsubClient` with better disconnection handling.

It exposes the same subscriptions as upstream (`account_subscribe`, `program_subscribe`,
`logs_subscribe`, `signature_subscribe`, `slot_subscribe`, `slot_updates_subscribe`,
`root_subscribe`, `block_subscribe`, `vote_subscribe`) and adds the reconnect behaviour the
SDK needs:

- On a dropped connection it reconnects and replays every live subscription, so callers
  keep their existing streams.
- A failed connect backs off for `2^(2+n)` seconds and retries up to three times, then
  panics rather than sitting on a dead client.
- It pings the server every 30 seconds and treats 60 seconds without any inbound message as
  dead, which forces a reconnect.
- `shutdown()` sends a normal close frame and ends the manager loop.
