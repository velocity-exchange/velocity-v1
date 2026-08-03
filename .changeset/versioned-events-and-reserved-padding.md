---
'@velocity-exchange/sdk': minor
---

Version velocity's own events and reserve tail space on the relay/router accounts.

`ProtocolUserWithdrawRecord` is renamed `ProtocolUserWithdrawRecordV0` (new on-chain discriminator) and is now wired into the event-subscriber surface — `EventMap`, the default `eventTypes` list, and the `VelocityEvent` union — where it was previously only a type mirror, so `EventSubscriber` never decoded it. Velocity's own events carry a `V0` suffix from here on: because an `#[event]`'s discriminator is derived from its struct name, a field addition ships as `…V1` with its own discriminator rather than changing the shape decoded under an existing name. Records inherited from upstream Drift keep their unversioned names.

Four zero-copy accounts grew a reserved tail (room for two future pubkeys each): `ClobCrankConditionsV0` 1,512 → 1,576 bytes, `QuoterCrossConditionsV0` 2,104 → 2,168, `UserConditionsV0` 7,608 → 7,672, `RouterQuoteBufferV0` 33,480 → 33,544. No field was added, removed, or reordered — only each struct's trailing `padding` array is longer — but `RouterQuoteBufferV0`'s header padding grows 12 → 76 bytes, moving `sources` / `levels` from offset 64 to 128, so hand-written decoders of that buffer must be re-derived.
