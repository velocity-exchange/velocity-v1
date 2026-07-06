---
"@velocity-exchange/usermap-server": patch
---

Fix usermap publisher gRPC health/liveness on low-activity markets. The subscribe request used `slots: {}` (an empty map = no slot subscription), so the health check — which only learns the current slot from incoming account writes — reported "slot lag" and resubscribed every 30s whenever user accounts were idle. Subscribe to the slot stream and advance the liveness markers from slot updates, so health tracks chain progress rather than account activity.
