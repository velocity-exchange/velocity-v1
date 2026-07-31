---
'@velocity-exchange/sdk': patch
---

Internal: relay resolvers share one harness and the condition accounts adopt relay-spec's `ConditionBlock` trait. No instruction signatures or account layouts change; the relay-spec dependency moves to the rev that adds the trait.
