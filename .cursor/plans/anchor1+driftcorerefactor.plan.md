---
name: Anchor1+DriftCoreRefactor
overview: Refactor the TypeScript SDK to support Anchor 1.0 while keeping the browser build compatible with Next 15 Turbopack (no app polyfills), and introduce a new DriftCore module as the minimal, mostly stateless source of truth for tx building, decoding, and PDAs that DriftClient builds on.
todos:
  - id: audit-anchor-usage
    content: Inventory all runtime imports of `@coral-xyz/anchor` in `sdk/src/**` and classify them as (a) purely types, (b) BN usage, (c) Program/Provider/coder usage, (d) misc utilities.
    status: completed
  - id: anchor1-deps
    content: Update SDK dependencies to Anchor 1.0 (`@anchor-lang/*`) and adjust imports for Node build; keep browser-safe files using type-only Anchor imports.
    status: completed
  - id: introduce-driftcore
    content: Add `sdk/src/core/` with `DriftCore` APIs for tx/instruction builders, decoding, PDAs, constants, and RPC fetch helpers (no subscriptions; pure-input state model).
    status: completed
  - id: driftclient-delegation
    content: Refactor `DriftClient` to delegate instruction building + decoding + PDA derivations to `DriftCore`, preserving existing subscriber/polling behavior as an optional layer.
    status: completed
  - id: browser-safety
    content: Remove Anchor runtime imports from the browser entry graph (especially `sdk/src/index.ts` BN export) and ensure browser build does not pull Node builtins, making it compatible with Next 15 Turbopack without app polyfills.
    status: completed
  - id: validate
    content: Build node+browser outputs and add a CI/smoke check to prevent Node builtin regressions in `lib/browser` (and optionally a Next/Turbopack import smoke test).
    status: completed
isProject: false
---

## Goals & constraints

- **Anchor 1.0 program toolchain**: `Anchor.toml` already targets Anchor `1.0.0`; the TS SDK should align its Anchor client layer accordingly.
- **UI constraint**: Your UI uses **Next 15 + Turbopack** and you want **no app-level polyfills**.
- **New architecture**: Add **DriftCore** as a minimal, mostly-stateless module that owns:
  - transaction/instruction building
  - account decode helpers
  - PDA derivations / address helpers
  - constants (markets/oracles/IDL-derived constants)
  - RPC fetch helpers (no websockets/subscriptions)

## Key finding: Anchor TS packages are not browser-safe by default

Both `@anchor-lang/core` (Anchor 1.0) and the current dependency `@coral-xyz/anchor` carry the same warning: they depend on Node.js native/core modules and **won’t work in Webpack 5 without polyfills** (and Turbopack generally won’t honor Webpack-style polyfill fallbacks). This means the SDK’s **browser entry** must avoid importing Anchor at runtime.

Today the SDK browser bundle can still “work” in some environments because it already relies on polyfill packages and keeps certain things optional, but **Next 15 Turbopack + no polyfills** requires a stronger guarantee: **no Node-core dependencies in the browser entry graph**.

## Architecture approach

### 1) Split the SDK into explicit layers

- **`DriftCore` (new)**: pure builders/decoders/PDAs/constants/RPC fetch helpers. No polling/websocket/subscriber state.
- **`DriftClient` (existing)**: keeps subscription lifecycle, polling/websocket/grpc integrations, user maps, etc.
  - Refactor `DriftClient` to call `DriftCore` for instruction construction, PDA derivations, and decoding.

### 2) Make browser safety a first-class build target

Keep two runtime entrypoints, with a strict rule:

- **Browser runtime** must not import Anchor packages (`@anchor-lang/core` / `@coral-xyz/anchor`) at runtime.
- Anchor may still be used for **types only** (via `import type`) so Node/bot users keep ergonomic types, while browser consumers don’t pull Anchor into the bundle.

This aligns with your existing isomorphic pattern documented in `sdk/src/isomorphic/README.md` and the current dual build outputs (`lib/node` and `lib/browser`) described by `sdk/package.json`.

## Concrete refactor steps (high level)

### A) Anchor 1.0 dependency alignment (Node + types)

- Replace SDK dependency on `@coral-xyz/anchor` with Anchor 1.0 packages (starting with `@anchor-lang/core`).
- Update all imports currently like `import { BN, Program, AnchorProvider, ... } from '@coral-xyz/anchor'` to their Anchor 1.0 equivalents.
- Keep any Anchor imports in code that must remain browser-safe as **type-only** imports.

Files most impacted:

- `sdk/src/driftClient.ts` (currently imports `* as anchor` and `AnchorProvider/Program/BN`)
- Many files importing `BN` from Anchor (see widespread imports in `sdk/src/**`)
- `sdk/src/decode/customCoder.ts` (uses `BorshCoder` etc)

### B) Introduce `DriftCore`

Create a new module surface, e.g. `sdk/src/core/` and export it from `sdk/src/index.ts`.

Suggested structure:

- `sdk/src/core/DriftCore.ts`

  - Stateless/pure APIs for:
    - instruction builders (return `TransactionInstruction`s)
    - transaction assembly helpers (return `Transaction` / `VersionedTransaction` _without sending_)
    - decode helpers (account buffers → typed objects)
    - PDA/address helpers (wrapping existing `addresses/pda`)
    - constants access (markets, IDL references, program IDs)
  - **State model** (per your choice): pure inputs only; no internal cache.
    - All builders accept `perpMarkets`, `spotMarkets`, `stateAccount`, etc. as arguments (or a single `DriftCoreContext` value passed through).

- `sdk/src/core/rpc.ts`
  - `fetchAndDecode*` helpers using `Connection.getAccountInfo` / `getMultipleAccountsInfo`
  - No subscriptions.

### C) Refactor `DriftClient` to delegate to `DriftCore`

- Move instruction-building logic out of `DriftClient` into `DriftCore`.
- `DriftClient` becomes mostly:
  - lifecycle + subscribers
  - state aggregation (markets/oracles/user accounts)
  - calls into `DriftCore` with the correct inputs to build instructions/txs.

### D) Make the browser entry graph Anchor-free

This is the crucial part for Next 15 Turbopack.

- Remove runtime re-exports/imports from Anchor in `sdk/src/index.ts` (today it imports and re-exports `BN` from Anchor).
- Replace `BN` usage strategy:

  - Prefer `bn.js` directly in browser-safe modules, or re-export `BN` as a _type alias_ (type-only) while using `bn.js` runtime.
  - The goal is: browser entry can do math/types without dragging Anchor.

- Apply the repo’s existing isomorphic pattern to any remaining “bad” runtime dependencies.
  - You already do this for `grpc` via `postbuild.js`; extend it as needed.

### E) Packaging/API compatibility

- Keep existing public exports working where possible.
- Add new exports:
  - `DriftCore`
  - `DriftCoreContext` (if you choose a single parameter object)
  - `core/*` helper exports
- Provide a migration note: UI apps should prefer `DriftCore` and manage subscriptions/state themselves; bots can keep `DriftClient`.

## Validation plan

- Ensure `sdk build` still produces `lib/node` and `lib/browser`.
- Add a lightweight “no-node-builtins” check for the browser build (e.g., ensure `lib/browser/**` does not contain `require('crypto')`, etc.).
- (Optional but recommended) Add a minimal Next 15 Turbopack “smoke import” test fixture to verify importing `@drift-labs/sdk` browser entry doesn’t error.

## Risks / tradeoffs (called out explicitly)

- **Direct Anchor exports vs browser safety**: Given the upstream note, a strict “no polyfills” browser target likely requires Anchor to be **type-only** in browser-facing modules, or avoided entirely at runtime.
- **Large surface area**: `BN` and `Program` usage is widespread; expect a multi-file refactor.
