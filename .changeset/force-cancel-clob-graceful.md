---
'@velocity-exchange/sdk': minor
---

Add `VelocityClient.getForceCancelClobOrdersIx`, the client for the CLOB arm of the
deteriorated-account sweep, with `ForceCancelClobRefV0` and `ClobSide` type mirrors.

The instruction now answers every "nothing to do" case with success instead of an error: the
account turned out healthy, the refs are already gone, or none of them is risk-increasing.
That is what lets a fill prefix it to clear a doomed maker out of its way while a keeper or
relay races to do the same work — whichever lands second is a no-op rather than a revert that
takes the fill with it. A latched authority-wide equity breaker is now grounds on its own, and
each ref declares the book side it rests on so the risk-reducing test can run before the cancel.
