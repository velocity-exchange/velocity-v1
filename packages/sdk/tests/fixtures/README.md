# Wire fixtures

One file per payload that crosses a language boundary, and the reason each is here rather than
written inline in a test.

A producer test that builds its own fixture and a consumer test that builds its own are blind to
each other: a field the producer stopped emitting is still in the consumer's literal, both suites
pass, and the wire is broken. So the fixture is the declaration, checked in once, and both sides
assert against it — the Rust producer that it emits exactly this, the TypeScript consumer that it
reads exactly this.

Changing the payload means changing the fixture, which fails whichever side was not changed with it.

| File | Produced by | Consumed by |
| --- | --- | --- |
| `userClobOrderRow.json` | `rust/book-publisher` (`user_orders::order_json_at`) | `packages/sdk` (`deserializeUserClobOrder`) |
