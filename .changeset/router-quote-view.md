---
'@velocity-exchange/sdk': minor
---

Add the `quoteRouter` view instruction and its `RouterQuoteBufferV0` account:
one simulated call returns per-source verified books (CLOB, PropAMM, DLOB, vAMM)
for a taker of a given direction and size, quoted in fill order so the vAMM's
last-look shading is already applied. Custom quoters' books are clamped to what
their user's margin supports, so published depth is fillable depth. Meant to be
simulated and read from post-simulation account state, not landed.
