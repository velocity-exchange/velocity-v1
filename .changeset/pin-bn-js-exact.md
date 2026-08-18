---
'@velocity-exchange/sdk': patch
---

Pin the `bn.js` runtime dependency to an exact version (`5.2.3`) instead of the `^5.2.0` range. `5.2.3` is the version the lockfile already resolved, so the SDK's own behaviour is unchanged; the effect is on consumers, who now resolve exactly `5.2.3` rather than any `5.2.x`. This makes `bn.js` consistent with every other runtime dependency in the package, all of which were already exact. Consumers that pull `bn.js` transitively at a higher patch may end up with a second nested copy, in which case `BN` instances will not share a constructor across the SDK boundary — add an `overrides`/`resolutions` entry to force a single copy if that matters for your tree.
