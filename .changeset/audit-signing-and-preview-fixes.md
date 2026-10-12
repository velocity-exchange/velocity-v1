---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

The swift order subscriber and the indicative quotes sender sign only an auth nonce of at most 64 alphanumeric characters. A longer nonce can be the hex of an order message, so the client refuses it and does not respond. When the server sends `auth_domain: "velocity-swift-auth:v1:"`, the client signs that prefix followed by the nonce. `SWIFT_AUTH_DOMAIN` exports the prefix. The admin CLI shows enum arguments, such as a market status, in dry runs and `multisig inspect`. The `market payloads` command reads `oracle_source` from the params file and refuses a source that is not Pyth Lazer.
