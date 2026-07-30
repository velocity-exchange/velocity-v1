---
'@velocity-exchange/sdk': minor
---

Trigger-order lifecycle over the CLOB: new `trigger_clob_order` keeper instruction (places a met trigger-limit onto the market's CLOB; the `User.orders` slot becomes a shadow holding the trigger params + CLOB order ref, freed on fill/cancel/expiry and re-armed edge-gated on eviction), new errors `OrderPlacedOnClob` / `OrderAwaitingTriggerRecross`, and `OrderBitFlag.PlacedOnClob` / `AwaitingTriggerRecross` mirrors. `cancel_order` refuses placed shadows — cancel them through `cancel_clob_order`.
