---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

A quoter can now say who its ladder stands on, so nothing off chain has to decode a book to find
out. `QuoterV0Account.quoteL3V0Discriminator` names the optional `quoteL3V0` leg of the quoter
interface; all-zero means the quoter does not implement it, which is every quoter that fills from
the one `user` its entry names, and a reader attributes that quoter's whole ladder to it. A book
implements the leg and answers per order.

`RouterQuoteBufferV0Account` carries the answer: a shared `rows` region of the new `QuotedRowV0`
(`price`, `size`, `orderId`, `authority`, `subAccountId`, `flags`), with `rowCount`,
`rowsTruncated`, and per-source `rowStart` / `rowLen` on `QuotedSourceV0`. The account grew from
33,544 to 41,736 bytes, so a buffer created against the old layout is too small and must be
recreated. `orderId` is zero on a row that is not an order, and bit 0 of `flags` marks a migrated
taker remainder — depth a cross cannot count on.

Who a quoter may settle for changed with it. A `Custom` entry is still held to the one account its
registration consented for. A `Clob` entry may now name any user the transaction already carries,
except the taker: velocity used to hold the response to the makers resting in the book's arena, and
the arena is that program's own state, so the check was the program against itself. What bounds a
book instead is that the entry *type* is a warm-admin decision at registration, a market's
`clobQuoter` is designated once and can never be pointed at a different entry, the response may only
name loaded users, every balance change is held to the quoted prices, and every user touched is
margin-checked after the fill.

`velocity-admin quoter init` takes `--l3-disc` for the new leg and `quoter update-config` can
withdraw it; `initializeQuoter` also reads the `state` account now, for the admin check on the type.

Two behavioural fixes ride along. A book's unknown-user grace window ages from `activationSlot`
rather than the slot the order was placed, so an auction-style order is no longer born past the
window — aged from placement, the first fill to see it would stop there and forfeit the depth
behind. And a book's L3 rows report every source the L2 ladder holds, the vAMM included (attributed
to the perp market, whose AMM settles it), ordered best price first and then by routing tier, which
is the order a shared price is actually filled in.
