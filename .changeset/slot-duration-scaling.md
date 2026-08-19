---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

Slot-duration scaling for the Solana slot-time reduction (400 -> 350 -> 300 -> 250 -> 200ms feature gates). New `State.slotDurationMs` field (0 = unset = 400ms baseline) and `updateStateSlotDurationMs` admin instruction (feature-gate values only, monotonic decreasing). All slot-denominated thresholds keep their 400ms-calibrated values and are scaled at read time; new `math/slots.ts` exports `sanitizeSlotDurationMs`/`effectiveSlots`/`effectiveSlotsCeil`/`baseUnitsFromSlots` mirroring the onchain `math::slots`. `getOracleValidity`, `isOracleValid`, `getSpotOracleValidity`, `calculateMaxPctToLiquidate`, and `User.canMakeIdle` take an optional trailing `slotDurationMs` (default 400). `SLOT_TIME_ESTIMATE_MS` is deprecated. Admin CLI gains `exchange set-slot-duration-ms`.
