---
'@velocity-exchange/sdk': patch
---

Remove `PerpFulfillmentMethod` from the IDL: perp fills route through the
single-pass router, so the fulfillment-method enum and the step loop it drove
no longer exist on chain.
