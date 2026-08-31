---
'@velocity-exchange/sdk': minor
---

Fire a DLOB stop-market straight to the book with `triggerOrderV1`.

`VelocityClient` gains `triggerOrderV1` and its `getTriggerOrderV1Ix` builder, plus
`VelocityCore.buildTriggerOrderV1Instruction`. Unlike `triggerOrder`, which flips a resting trigger
live and leaves it for a later fill crank, `triggerOrderV1` fires a trigger-market order and fills
it against the book in the same instruction, resting only the remainder as a taker-origin order.
Nothing lingers live in `User.orders`. For DLOB trigger-market orders; a trigger-limit keeps its own
`triggerClobOrder` path.

`PerpPosition` gains `reduceOnlyClobOrders`: the count of reduce-only orders the owner has resting on
the CLOB. The router caps a user's reduce-only fills to the position they reduce only while this is
non-zero, so a reduce-only stop can rest its remainder on the book without over-filling.
