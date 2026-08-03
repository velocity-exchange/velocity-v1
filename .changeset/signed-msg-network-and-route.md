---
'@velocity-exchange/sdk': minor
---

Signed-msg orders carry a network tag and an optional signed route. `SignedMsgOrderParamsMessage` / `SignedMsgOrderParamsDelegateMessage` gain two trailing optional fields: `network` (`SignedMsgNetwork.MAINNET`/`DEVNET`, `'m'`/`'d'`) which the program validates against its own cluster — the signature covers the order and not the chain, so without it a devnet order replays verbatim on mainnet — and `route`, the `QuoterV0` entry keys of the custom quoters (PropAMMs) the taker wants their order routed through. The CLOB and the vAMM are every router fill's mandatory baseline, so they are implicit and never listed; a route naming more than four quoters is refused rather than truncated. Both are optional and the verifier zero-pads short payloads, so existing producers keep working unchanged — but they should start sending the network tag.

Also new in this release: `HotRole.FlowAuthority` and `StateAccount.hotFlowAuthority` (the retail-flow attestation key), and `place_clob_order`'s trailing optional `instructionsSysvar` account, which is required when requesting a faster-than-default CLOB activation delay — that now needs the transaction co-signed by the flow authority (`UnattestedFastActivation`).
