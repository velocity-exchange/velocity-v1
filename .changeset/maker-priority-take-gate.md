---
'@velocity-exchange/sdk': minor
---

Maker priority: on a book with an activation speed bump, only attested flow fills synchronously.

With a nonzero `default_activation_delay_slots`, a transaction not co-signed by the flow
authority cannot fill against the book in the same transaction. `placeAndTakePerpOrder` (v1)
and signed-message orders rest the whole order taker-origin through the default window instead,
and the cross cranks fill it; an IOC or a success condition on such a take is refused with
`UnattestedSynchronousTake` (6403). Keeper fills still run but the book quotes them no depth, so
they reach the vAMM and DLOB makers only. Cancels are never delayed, so a maker can always
reprice ahead of unattested aggression. Books with a zero default delay are unaffected.

Attestation has two transports: `placeAndTakePerpOrderV1`, `placeAndMakePerpOrderV1` and
`modifyOrderV1` carry an optional `flowAuthority` signer account, and
`placeSignedMsgTakerOrder` gains a `flowAttestation` argument — swift's detached signature over
the order's own signature plus an expiry, verified in-program. The flow authority never signs a
keeper-built transaction, and an attested fill pays no second signature fee.

The quoter wire carries the verdict: `QuoteArgsV0`/`ExecuteArgsV0` gain `taker_served_window`,
set by velocity for attested flow, and for the protocol cranks only when the orders they settle
rested at least two slots — a zero-delay book cannot launder fresh flow into the flag by
place-then-crank. The midpoint's
`require_attested_flow` checks that flag instead of the instructions sysvar, and its
`quote_v0`/`execute_v0` account lists drop the sysvar and velocity-State accounts — midpoint
quoter entries must be re-registered with the shorter legs.

`quoteRouter` takes the same fact as an argument (`taker_served_window`): a view for unprotected
flow shows no depth from a bumped book or a protected quoter, matching the fill's route.

`QuoterV0Account` gains `bookTickSize`, `bookMinOrderSize` and
`bookDefaultActivationDelaySlots` — the book's placement rules, mirrored onto the entry by
`updatePerpMarketClobQuoter` so the fill and placement paths stop CPI'ing `order_rules_v0`
entirely — the attach is the one remaining reader. Re-run the attach
after changing a book's rules; its `quoter` account is now writable.
