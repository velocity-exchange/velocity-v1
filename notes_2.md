Same as before in notes.md. Parallelize where possible:


Yes, I'm fine with validation cost on CLOB.

Option<Vec<UserRefV0>> is inefficient. We should change that to be an actual 0 copy array, and doesn't need to be an option. If it's empty treat as None.

If events are returning vecs, that seems not great. There a way around this to save CUs?

One pre-existing bug it found and left alone (config surface, your call): EXECUTE_USERS_CEILING is derived at 48 bytes per user, but a
UserBalanceChange record is 54 bytes even with zero order ids — so a market configured at the ceiling fails with ResponseTooLarge. Bruh fix this shit.

We need to make sure we actually validate the users returned in execute. In general address this convo stuff:

Renato Marziano  [6:53 AM]
Hey @Noah Prince, nice to meet. I went over the new design doc you sent here and wanted to point out a few things, especially around the assumptions that quoters will misbehave:

- some edge cases around what the quoters actually execute need extra caution:
    - what if the included quoter program decides to revert? Even worse, it might decide to do this strategically by using ix introspection. in general we cannot catch the panic, but something we could do is prove that a given binary always succedes (at the same time, drainage of CU would accomplish the same result)
    - what if I run two quoters that are somehow interdependent on each other? say i point my Midpoint quoter's hot_authority at a pda of a second quoter program i own. the second one executes first and CPIs set_levels_v0 to reprice or zero the first one after seeing the whole split. this adds a layer of complexity to the next point stated below

- I don't see how the design binds the quoted price to the executed price. UserBalanceChange carries quote_size, which is the price, and its picked by the quoter.
    - a discrepancy here would lead to a few issues. first, the offchain /route path reading quotes to know which N accounts to include would not necessarly give you the guarantee that the included quoter accounts actually fill anywhere near what they showed. the account set is picked off prices that arent binding, so by the time the tx lands the quoters in it are free to charge whatever they want atm.
    - the direction of the execute contract seems backwards to me. velocity passes size and the quoter returns the price, since quote_size in UserBalanceChange is the notional. it should pass the price you were quoted at and let you return the size you can still fill, if any. as written theres nothing to compare the returned price against, so a quoter whose market genuinely moved and one thats just charging more look identical to velocity. validate_fill_price on velocity-v1 today takes the limit price as an input and runs against both the taker and the maker limit on every match

- theres an adverse incentive for rational quoters to maximize their needed accounts ( quote_accounts, execute_accounts ) to MAX_QUOTER_ACCOUNTS=32, since that reduces the amount of competing quoters that can be included in a tx. this is something the filler needs to be aware of



- balance_changes only has to name users in the loaded set, but that set includes the taker and every other quoter's makers. so quoter A can mint a position onto quoter B's maker, or onto the taker, at a price of its choosing. for Custom the registry has exactly one user field so the rule should be that they can only emit changes for that user.

- velocity_signer is still passed as a signer in invoke_quoter. i saw @Robert Chen flagged the general signer forwarding thing and you dropped is_signer from AmmAccountMeta, but this is still there .that key is the spl token authority on every spot_market_vault and insurance_fund_vault (the initialize_token_account calls in admin.rs), and signer privilege is inherited by the callee, so a quoter that gets a vault into its account list can forward it to the token program.

- is_approved also needs to be able to handle upgradable programs, so it must require a frozen upgrade_authority_address , otherwise we're just trusting the MMs programs.

also a few things about the pseudo impl: didnt look too deep here since id guess the code is illustrative, but some stuff def needs fixing :

- quote()/execute() only validate is_active && is_approved, not self.market == market_index
- nothing rejects price == 0 or unsorted levels on the response even though the type says best
  price first. PriceLevel { price: 0, size: u64::MAX } wins the entire waterfall. the clob's
  own place() rejects price 0, the ingestion of a foreign response doesnt
- place_authority guards placement but nothing is said about who can call clob execute. if
  its not gated to the velocity pda anyone can call it and wipe the book with no positions
  created anywhere
- const_assert_eq!(size_of::<ClobMarketV0>(), 90256) doesnt match the struct, i get 90288
  (1024*88 arena + 176 header), pretty minor though

cc @Robert Chen
1 replyNoah Prince  [9:31 AM]
Going through one by one


Quoters are permissioned. We must approve each one. If we catch one reverting, they'll just get banned. This is how propAMMs work now. I don't think we can prove a given binary always succeeds, isn't this basically the halting problem?
I don't see how this is a problem. Order levels are returned regardless of the CPI/quote retreat. And the order either gets filled within slippage or doesn't. If it doesn't, and this is happening often, we stop routing to that propAMM and probably ban them.
Within a transaction, the quoted price is bound to the executed price. The router within velocity checks this. In terms of remote routing, there are no guarantees because the market can move while your tx is still in flight. That's why you have things like slippage. The hope is that by having the router include more AMMs, they're forced to be competitive even as the market moves (ie they can't just maliciously take you for up to max slippage). Again, the final guard here is routing and deprioritization or banning.
Maximizing accounts right now would lead to just not being approved. Maybe eventually we need to add fees per account.
Yes balance_changes will need validation that it's only users you're approved to do stuff against.
The signer will be a different signer than the vault authority to avoid any signer forwarding issues.
I think it's fine for MMs to upgrade their programs. If they start behaving maliciously (reverting, etc) we stop routing to them. The key is limiting the blast radius of maliciousness. Like making sure a malicious one can't steal money.