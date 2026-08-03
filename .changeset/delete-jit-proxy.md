---
'@velocity-exchange/sdk': minor
---

Delete jit-proxy. The `@velocity-exchange/jit-proxy` package is unpublished and the jit-proxy program (`J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ`) is removed from the repo — the CLOB's activation-slot auction is the taker protection that replaced the JIT auction, so a former JIT maker rests orders on the book and the router matches them. There is no successor package; drop the dependency. In this SDK the only surface change is the `JIT_PROXY_PROGRAM_ID` field, which is gone from `VelocityConfig` and from both env config presets.
