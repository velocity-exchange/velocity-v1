---
'@velocity-exchange/jit-proxy': minor
'@velocity-exchange/sdk': minor
---

Point jit-proxy at Velocity's own program deployment `J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ` (devnet & mainnet), replacing Drift's upstream `J1TnP8zvVxbtF5KFp5xRmWuvG9McnhzmBd9XGfCyuxFP`: the jit-proxy IDL/types are regenerated with the new address and the SDK config presets' `JIT_PROXY_PROGRAM_ID` now resolve to it. Also fixes `JitProxyClient` deriving the builder-order `REV_ESCROW` PDA under the jit-proxy program id instead of the velocity program id (the account passed for `hasBuilder` orders was wrong), and stops passing `velocityProgram` explicitly now that the IDL pins its address.
