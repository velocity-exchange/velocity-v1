---
'@velocity-exchange/admin-cli': minor
---

Add `batch` command: build several IDL-driven instructions from one payload file ({ instructions: [{ ix, args, accounts }, ...] }, each entry the `call` payload shape plus the instruction name) and dispatch them as a single transaction or vault proposal, so related admin changes share one approval round and one timelock.
