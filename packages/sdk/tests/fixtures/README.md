# Wire fixtures

One file per payload that crosses a language boundary.

A producer test that builds its own fixture and a consumer test that builds its own are blind to
each other. A field the producer stopped emitting stays in the consumer's literal, both suites
pass, and the wire is broken. The fixture is the one declaration instead. The Rust producer asserts
that it emits exactly this, and the TypeScript consumer asserts that it reads exactly this.

A change to the payload means a change to the fixture. That fails whichever side was not changed
with it.

| File | Produced by | Consumed by |
| --- | --- | --- |
| `userClobOrderRow.json` | `rust/book-publisher` (`user_orders::order_json_at`) | `packages/sdk` (`deserializeUserClobOrder`) |
