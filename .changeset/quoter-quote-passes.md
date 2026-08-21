---
"@velocity-exchange/sdk": patch
---

Regenerate the IDL for `quote_router`'s new `include_vamm` argument. A market
with more quoters than one view can carry is now read in several passes, and
the flag names the pass that carries the vAMM. No TypeScript surface changes:
the view has no instruction builder in the SDK.
