# Code shape

Read this before you write, split or restructure Rust code. It covers how big a function or file
gets, how to verify a refactor, when not to restructure, and Rust style.

## Function and module shape

The reference for this is `controller/orders/`: one 7,967-line file became a 343-line module root
over nine subject modules, and its fill path went from four functions of 571, 196, 155 and 331
lines to a chain whose largest step is 70.

**Size limits.** A function body stays around 60 lines or under, and takes six arguments or fewer.
These are not arbitrary: a body you cannot see at once hides its own control flow, and a long
argument list is almost always a context struct that has not been written yet. Exceeding either is
allowed, but say why in a comment at the definition.

**Carry context in a struct; put the validating on it.** When several steps need the same maps,
market, clock and seats, that is a type. Give it a constructor in the shape of `AccountMaps::new`
and make each step a method, so a step takes the context and its own few arguments and nothing
else. Prefer several small contexts over one wide one: a struct that accumulates every lifetime in
the call graph becomes its own obstacle. `PerpFill` went from 8 lifetimes and 25 fields to 4 and 19
by moving the external-book concern into `ExternalVenue` and the running totals into `FillTally`.

**Name a layer for what it governs, not for what it does to the data.** A chain that read
`fill_perp_order` to `fulfill_perp_order` to `route_and_settle_perp_fill` told a reader nothing:
three synonyms, and each layer had exactly one caller, so the layering carried no reuse either.
Those layers govern the order, the taker's risk limits, and liquidity, and they say so now. If two
functions in a chain could swap names without anybody noticing, the names are wrong.

**One subject per file.** A file is the right size when a reader who opens it finds one subject. A
module root holds the doc, the imports, the `mod` declarations, the re-exports, and only what
several subjects genuinely share. Re-export every public name the old file exported, so a split
changes no caller's imports.

**Some long signatures are deliberate, and stay.** `emit_perp_action_record` keeps its long
parameter list because a struct there makes the caller build the 480-byte record in its own frame
and trips the SBPF stack-overwrite check. `FillerSide` keeps four lifetimes because `&mut &mut` is
invariant. An instruction handler keeps its argument list because that list is the program's ABI.
Each of those carries a comment saying so. Do not "fix" them.

## Verifying a refactor

A refactor that changes behaviour is a rewrite, and the unit test count is the proof it did not:
**the count must be identical before and after.**

- **`cargo check` and `cargo clippy` without `--all-targets` build only the lib.** They report clean
  over a test tree that does not compile. Always pass `--all-targets`.
- **Run each verification command on its own.** Chaining them has produced output that reported
  compile errors at line numbers which did not exist in the file, and has hidden a real `fmt`
  failure that was then reported as clean.
- **Diff the compiler's warning population against a clean `HEAD` worktree**, bucketed by
  `(level, file, message)` so line numbers do not matter. This has caught several real regressions
  that the tests did not, including a settle path reading the raw book instead of the clamped
  ladder a quoter was allocated against, and a shared maker-seat step using the strict position
  lookup where one caller needs the creating one.
- **Measure every function, not the one that improved.** A report that the inner pass reached 155
  lines was true and useless while its three siblings sat at 571, 196 and 331.
- **Verify a re-export by compiling it.** Generate a temporary module that imports every public
  item of the old file by path, compile it under the feature flavors that include the gated names
  and through the path `lib.rs` actually uses, then delete it. Reading the `pub use` list proves
  nothing.

## Refactoring against an audit

A split moves code, so it destroys the feature diff. An auditor reading `master..<branch>` for a
file that was split sees the file deleted and new files appear, with the branch's own changes
scattered inside them. Weigh that before restructuring a file the branch already changed: the cost
is not the new code, it is the diff that can no longer be read. Land readability work only on files that are majorly changed or created in the feature. For example, if you only slightly change liquidation, you should not break up the entire file into modules. But if you overhaul orders, you break it into modules.

## Rust style

Prefer declarative iterator chains (`map`/`filter`/`fold`/`try_fold`/`collect`) over imperative `for`/`while` loops wherever the two are performance-equivalent. Explicit loops are fine when they are genuinely better: hot paths where the imperative form saves real work, or indexed mutation across parallel structures that the borrow checker won't allow through closures. Also avoid redundant recomputation in loops — hoist or precompute values that don't change (or change predictably) across iterations.

That last rule is about recomputation, not about plain field reads. **Do not copy a field into a
local for its own sake.** `let min_order_size = self.min_order_size;` at the top of a function,
used once sixty lines below, costs a reader a lookup and buys nothing. Read `self.min_order_size`
where the value is used, or `book.min_order_size` inside a `walk_side` closure, which receives the
market as its first argument for exactly this reason.

It does not save compute. It spends it. Removing these locals from `quote`, `quote_l3` and
`execute` moved `quote(full side)` from 16439 CU to 16237 and `execute(50 orders)` from 45639 to
45633, measured with `cu_benchmarks` in `anchor-v2/programs/clob/tests/clob_tests.rs`. A local a
walk closure captures stays live across every iteration and every call the body makes. A field
read at the point of use folds into the instruction that needs it and leaves the closure's
environment smaller, which matters on a 4 KB SBF frame.

Two cases still earn the local. Keep it on the line before its use, not at the top of the
function:

- The borrow checker refuses the field read. `l3_row_flags(node, book.blocking_min_size, ..)`
  inside a call that already takes `&mut book.response` does not compile.
- The value has to be read before something changes it, such as `let order_id =
self.next_order_id;` before the counter increments.
