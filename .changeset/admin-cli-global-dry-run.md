---
'@velocity-exchange/admin-cli': minor
---

Make `--dry-run` global, validate `call` and `batch` payloads against the IDL, and decode
instructions in the dry-run output.

Each command declared `--dry-run` itself, so 13 had it and the rest did not. `call`,
`spot-market set-max-token-deposits` and `perp-market deposit-fee-pool` offered no way to preview
a proposal. It is now a global option that `sendOrPropose` checks, and `sendOrPropose` is the only
code path that signs or proposes. Gating it there covers every state-changing command, so no
command can accept the flag and still send. This removes the 13 local declarations. Commands that
print a fuller preview, such as `wallet swap` with its quote and `lut extend` with its account
diff, return before dispatch and behave as before.

`buildIxFromPayload` now rejects unknown and missing args. It used to pass `undefined` to Anchor,
which serialized a missing numeric arg as `0`. The IDL names fields in snake_case while the Anchor
client exposes them in camelCase, so a payload written from the IDL proposed
`initializePythLazerOracle` with `feedId` 0, and a seeds constraint caught it only later. The
check runs in both directions. An `{ option: T }` arg may still be absent.

The dry run now decodes each instruction and prints its name, arguments and named accounts,
instead of a program id and an account count. A wrong argument shows up before the proposal
exists. It still prints the account count for every instruction, including ones it cannot decode,
because that count is the only warning that a Jupiter route is large enough to overflow the Squads
executor.
