---
'@velocity-exchange/sdk': minor
---

Trigger orders as relay conditions: `triggerOrder`/`triggerClobOrder` gained two optional trailing accounts (`triggerConditions`, `crankConditions`) — the SDK's trigger builder emits program-id placeholders automatically, and their `authority` is now a payout account in program-keeper mode (the signed keeper path is unchanged). New IDL surface: `syncTriggerConditions` (permissionless per-user condition sync), simulation-only `resolveTriggerOrder`/`resolveTriggerClobOrder`, and the `TriggerConditionsV0` account.
