# Type discipline

Read this before changing program logic, arithmetic, validation, account layouts, or SDK math
that mirrors the program. These rules apply to new logic and logic you change. Editing a function
does not require migrating its unrelated code or callers. Preserve existing behavior and the ABI
unless the task calls for a change. Report adjacent problems separately.

A proof type is a type whose construction validates a specific invariant. Its design must state
what it proves, which inputs and state it applies to, and what invalidates it. Types such as
`Amount<Quote>`, `Fresh<T>`, `Committed`, and `Distinct` are design examples, not APIs this
document requires you to find or introduce across the repo.

## Rules for new or changed code

### Arithmetic and conversions

- Use `SafeMath` from [`safe_math.rs`](../../programs/velocity/src/math/safe_math.rs) for fallible
  financial arithmetic. Use checked wider intermediates when needed. Do not introduce bare
  arithmetic on balances, prices, fees, interest, or precision conversions without a local proof
  covering every intermediate and divisor. Use compile-time assertions when the bounds are static.
- Use `.cast()?` from [`casting.rs`](../../programs/velocity/src/math/casting.rs) or `TryFrom`
  for fallible integer conversions. Use `From` for lossless widening. Do not add integer `as`
  conversions. Preserve the existing error mapping when replacing a conversion.
- Make rounding explicit in new financial helper names or a rounding enum. The existing
  `safe_div`, `safe_div_floor`, and `safe_div_ceil` have different signed rounding behavior.
  Check the operand signs and the economic effect before selecting one. Keep existing conversion
  helpers' rounding behavior when using them, including their `round_up` arguments.
- Reuse precision and policy constants in `math/constants.rs`. Do not invent new limits only
  to justify unchecked arithmetic. Do not substitute saturating arithmetic for an error unless
  clamping is the intended behavior.

### Validation and state

- Use Anchor account constraints for identity checks as described in
  [`AGENTS.md`](../../AGENTS.md#instruction-layout). Parse native instruction bytes and remaining
  accounts in their boundary helpers. New domain helpers take the smallest typed inputs they need.
  Keep raw `AccountInfo` handling in account parsers, CPI wrappers, and account invariant checks.
  Anchor account types prove only their framework checks, not business invariants on their fields.
- Keep oracle and interest gates required by the operation. Use the shared helpers in
  [`math/oracle.rs`](../../programs/velocity/src/math/oracle.rs),
  [`state/oracle_map.rs`](../../programs/velocity/src/state/oracle_map.rs), and
  [`math/margin.rs`](../../programs/velocity/src/math/margin.rs). Follow the oracle rules in
  [`AGENTS.md`](../../AGENTS.md#oracle-usage). A decoded price alone does not prove validity.
- Check external inputs before domain logic. Keep state-dependent checks where the relevant state
  is available, and retain final accounting assertions. Remove a repeated check only when a type
  or the operation's structure enforces the same invariant for every caller.
- Revalidate after local mutations or CPIs invalidate a checked fact. Release account-data borrows
  before a CPI that needs mutable access. Reload owned account copies or reacquire and validate
  account views before using changed data. Do not introduce unchecked CPI calls.
- When an operation requires different accounts, check their keys in the shared validation path.
  Two typed accounts can refer to the same account. Sequential mutable borrows do not prove
  distinctness after the first borrow ends.
- Match enums exhaustively. Group explicit variants when they share behavior. The crate denies
  `clippy::wildcard_enum_match_arm`, so a `_` arm on an enum fails clippy and CI. `cargo build`,
  `cargo test` and `bun run program:build` still accept one, so run clippy. A fallback that
  rejects unknown raw bytes at a parser boundary matches on the integer, not the enum, and is
  fine. Preserve persisted enum discriminants and error codes.
- Use an enum for mutually exclusive lifecycle states or flags that permit invalid combinations.
  A boolean for an independent binary property is fine. Do not change persisted flags into enums
  without reviewing the layout and client compatibility.
- Treat a new "caller must ensure" comment as a missing enforced precondition. Use an existing
  validated type, a checked shared helper, or an assertion backed by a proof. If this needs a wider
  migration, retain an executable check and describe the remaining limitation in the PR.
- Return errors for invalid external input. Do not add production `unwrap`, `expect`, or panic
  paths for values supplied by callers or accounts. Assertions in tests are appropriate.

### Layout and behavioral compatibility

- Add constant offset assertions for new or moved zero-copy fields and existing fields whose
  offsets the edit could affect. Retain size assertions and check target-specific alignment.
  A new zero-copy struct needs assertions for its size, alignment, and every field offset.
  Follow [`alignment-and-native-offsets.md`](../alignment-and-native-offsets.md) and verify the
  host and SBF builds. A host-only size check does not prove onchain layout compatibility.
- Keep bounded proof types out of POD account storage. Store the existing raw representation and
  validate it into a separate view. Do not add version bytes, require reserved bytes to be zero,
  reorder fields, or change discriminators as incidental cleanup.
- Pin affected ABI bytes when changing account representation or names. Check existing
  discriminators before declaring explicit constants. Preserve deployed values and verify SDK
  decoding against the same fixture. A fixture generated by the current serializer alone does
  not prove compatibility with deployed accounts.
- Follow the SDK mirroring rules in `AGENTS.md`. Shared Rust and SDK spread fixtures already exist
  in [`sdkParity/fixtures`](../../packages/sdk/tests/sdkParity/fixtures/README.md). Extend this
  pattern for changed formulas. Derive expected values from the program, review them, and test
  rounding edges and economic invariants independently. Matching implementations can share a bug.

## Rules for new domains and explicit migrations

Apply these when designing a new instruction domain or undertaking an agreed migration, including
new code in a VLP extraction. Moving existing code alone does not require redesigning it.

- Use small unit newtypes where amounts, prices, scales, or time units cross domain function
  boundaries and can be confused. Keep raw instruction and account representations compatible.
  Convert at the boundary. Do not create a generic units framework for a single helper.
- Use `NonZero` for divisors that must be nonzero. Use validated bounded types only for actual
  protocol limits. Keep checked arithmetic for bounds that depend on live state.
- Give proof types private fields and one validating construction path. Do not derive `Default`,
  deserialization, or `Pod`, or add infallible raw conversions that bypass validation. Methods
  producing another value must preserve the invariant or validate again. Do not use `unsafe` as a
  label for a business invariant that callers can bypass.
- Bind proofs to the data they validate. Oracle validity depends on the market, action, risk
  configuration, and observation time. Prefer a validated context that owns or borrows the
  relevant data over a detached token that callers can pair with another market. When a proof
  depends on user positions as well as market state, a mutation of either invalidates it.
- Make the sensitive operation require the validated input. For example, a settlement helper takes
  a validated oracle context, not a raw `OraclePriceData`. Remove public raw-input alternatives to
  that operation. Do not unwrap the proof back into unrelated integers before the operation that
  relies on it.
- Use typestate only where it makes illegal transitions uncallable without obscuring persistence.
  Test write-back to the correct account. A completion token alone does not prove this happened.
- Test constructor bounds and rejection cases. New byte parsers need arbitrary-input no-panic and
  valid-value round-trip tests. Add compile-fail tests for the misuse the new proof API is intended
  to prohibit. Test invalidation and the relevant operation's accounting behavior as well. No
  program crate depends on `proptest` or `trybuild` yet. Adding them as dev-dependencies is part of
  this work, so add them rather than skipping the tests.

## Enforcement

Lint configuration and CI commands belong in code and [`testing.md`](./testing.md). Not every rule
here is compiler-enforced. When a lint becomes blocking, follow its scope and allow exceptions only
with a concrete justification at the allow site. Keep release overflow checks enabled.
