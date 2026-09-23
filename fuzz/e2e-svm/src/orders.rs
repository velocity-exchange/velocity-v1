//! The v1 order surface: every perp order routes through the market's book.
//!
//! A limit order that posts rests on the book through `place_and_make_perp_order_v1`.
//! Any other order takes through `place_and_take_perp_order_v1`, and an unfilled
//! restable remainder rests on the book. A trigger order waits in a `user.orders`
//! slot through `place_trigger_orders_v1` until a trigger instruction fires it.
//!
//! A fill settles only against the makers the transaction carries, so a take
//! carries every other fuzz user as a maker, then the market's book entry.
//!
//! The account lists come from the program's own `Accounts` structs, so an
//! account the program adds or renames breaks this file at compile time.

use {
    crate::{compute_budget_ix, instructions_sysvar_id, velocity_program_id, Fixture, NUM_USERS},
    anchor_lang::{InstructionData, ToAccountMetas},
    crucible_test_context::TxOutcome,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_signer::Signer,
    velocity::{
        instructions::{
            CancelOrderV1Params, CancelOrdersV1Params, CrankClobEvictArgs,
            CrankClobRemoveExpiredArgs, ForceCancelClobOrdersArgs, ModifyOrderV1Params,
            PlaceAndMakePerpOrderV1Args, PlaceAndTakePerpOrderV1Args, PlaceTriggerOrdersV1Args,
            TriggerLimitOrderV1Args, TriggerMarketOrderV1Args,
        },
        state::{
            order_params::{OrderParams, PlaceAndTakeOrderSuccessCondition},
            prop_amm::{ClobCancelSides, ClobOrderRefV0, ClobSide},
        },
    },
};

/// Enough for a take that CPIs into the book and settles several makers.
const ORDER_COMPUTE_UNITS: u32 = 1_400_000;

/// Book orders remembered at once. The oldest is dropped past this.
const MAX_TRACKED_BOOK_ORDERS: usize = 32;

/// Who an instruction rests a book order for, and on which side.
#[derive(Clone, Copy)]
pub struct Placement {
    pub owner_idx: usize,
    pub side: ClobSide,
}

/// An order resting on the book, as its placement reported it.
#[derive(Clone, Copy)]
pub struct BookOrder {
    pub owner_idx: usize,
    pub order_ref: ClobOrderRefV0,
    pub side: ClobSide,
}

impl Fixture {
    /// Every user but `taker_idx`, as the `(User, UserStats)` pairs a fill
    /// settles against.
    fn maker_metas(&self, taker_idx: usize) -> Vec<AccountMeta> {
        (0..NUM_USERS)
            .filter(|idx| *idx != taker_idx)
            .flat_map(|idx| {
                let maker = &self.users[idx];
                [
                    AccountMeta::new(maker.user_pda, false),
                    AccountMeta::new(maker.stats_pda, false),
                ]
            })
            .collect()
    }

    /// The remaining accounts of a routed order: the markets, the makers, then
    /// the market's book entry.
    pub(crate) fn route_remaining_accounts(&self, taker_idx: usize) -> Vec<AccountMeta> {
        let mut accounts = self.market_ras(true);
        accounts.extend(self.maker_metas(taker_idx));
        accounts.extend(self.clob.route_metas());
        accounts
    }

    pub(crate) fn place_and_take_ix(
        &self,
        user_idx: usize,
        params: OrderParams,
        success_condition: Option<PlaceAndTakeOrderSuccessCondition>,
    ) -> Instruction {
        let user = &self.users[user_idx];
        let mut accounts = velocity::accounts::PlaceAndTakeV1 {
            state: self.state_pda(),
            user: user.user_pda,
            user_stats: user.stats_pda,
            authority: user.keypair.pubkey(),
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
            flow_authority: None,
        }
        .to_account_metas(None);
        accounts.extend(self.route_remaining_accounts(user_idx));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::PlaceAndTakePerpOrderV1 {
                args: PlaceAndTakePerpOrderV1Args {
                    params,
                    success_condition,
                },
            }
            .data(),
        }
    }

    pub(crate) fn place_and_make_ix(
        &self,
        user_idx: usize,
        params: OrderParams,
        activation_delay_slots: Option<u32>,
    ) -> Instruction {
        let user = &self.users[user_idx];
        let mut accounts = velocity::accounts::PlaceAndMakeV1 {
            state: self.state_pda(),
            user: user.user_pda,
            user_stats: user.stats_pda,
            authority: user.keypair.pubkey(),
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
            flow_authority: None,
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(true));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::PlaceAndMakePerpOrderV1 {
                args: PlaceAndMakePerpOrderV1Args {
                    params,
                    activation_delay_slots,
                },
            }
            .data(),
        }
    }

    pub(crate) fn place_trigger_orders_ix(
        &self,
        user_idx: usize,
        params: Vec<OrderParams>,
    ) -> Instruction {
        let user = &self.users[user_idx];
        let mut accounts = velocity::accounts::PlaceTriggerOrdersV1 {
            state: self.state_pda(),
            user: user.user_pda,
            authority: user.keypair.pubkey(),
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(false));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::PlaceTriggerOrdersV1 {
                args: PlaceTriggerOrdersV1Args { params },
            }
            .data(),
        }
    }

    /// Send `ixs` in one transaction behind a compute-unit raise. When the
    /// transaction places an order, remember the book order it reports.
    pub(crate) fn send_order_ixs(
        &mut self,
        ixs: Vec<Instruction>,
        signer: &Keypair,
        placement: Option<Placement>,
    ) -> bool {
        let queued = std::iter::once(compute_budget_ix(ORDER_COMPUTE_UNITS))
            .chain(ixs)
            .all(|ix| {
                self.ctx
                    .raw_call(ix)
                    .signers(&[signer])
                    .add_transaction()
                    .is_ok()
            });
        if !queued {
            return false;
        }

        match self.ctx.send_batch() {
            Ok(Some(outcome)) if outcome.is_success() => {
                if let Some(placement) = placement {
                    self.remember_book_order(placement, &outcome);
                }

                true
            }
            Ok(Some(outcome)) => {
                if std::env::var_os("FUZZ_DEBUG").is_some() {
                    eprintln!("tx failed:\n  {}", outcome.logs().join("\n  "));
                }

                false
            }
            _ => false,
        }
    }

    /// A placement that rests leaves the book's own answer as the return data:
    /// the order's `(node_index, order_id)`.
    fn remember_book_order(&mut self, placement: Placement, outcome: &TxOutcome) {
        let returned = outcome.return_data();
        if returned.program_id != self.clob.program || returned.data.len() != 12 {
            return;
        }

        let order_ref = ClobOrderRefV0 {
            node_index: u32::from_le_bytes(returned.data[..4].try_into().unwrap()),
            order_id: u64::from_le_bytes(returned.data[4..12].try_into().unwrap()),
        };
        self.book_orders.push(BookOrder {
            owner_idx: placement.owner_idx,
            order_ref,
            side: placement.side,
        });
        if self.book_orders.len() > MAX_TRACKED_BOOK_ORDERS {
            self.book_orders.remove(0);
        }
    }

    /// A remembered book order, if any. It may have filled or left the book
    /// since, which the instruction it is handed to reports.
    pub(crate) fn pick_book_order(&self, nth: usize) -> Option<BookOrder> {
        (!self.book_orders.is_empty()).then(|| self.book_orders[nth % self.book_orders.len()])
    }
}

/// Instructions that act on the book on a user's behalf.
impl Fixture {
    pub(crate) fn cancel_book_order_ix(&self, order: BookOrder) -> Instruction {
        let user = &self.users[order.owner_idx];
        let mut accounts = velocity::accounts::CancelOrderV1 {
            user: user.user_pda,
            authority: user.keypair.pubkey(),
            perp_market: self.perp_market_pda,
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(false));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::CancelOrderV1 {
                params: CancelOrderV1Params {
                    market_index: crate::clob::MARKET_INDEX,
                    order_ref: order.order_ref,
                },
            }
            .data(),
        }
    }

    pub(crate) fn cancel_book_side_ix(
        &self,
        user_idx: usize,
        sides: ClobCancelSides,
    ) -> Instruction {
        let user = &self.users[user_idx];
        let mut accounts = velocity::accounts::CancelOrdersV1 {
            user: user.user_pda,
            authority: user.keypair.pubkey(),
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(false));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::CancelOrdersV1 {
                params: CancelOrdersV1Params {
                    market_index: crate::clob::MARKET_INDEX,
                    sides,
                },
            }
            .data(),
        }
    }

    pub(crate) fn modify_book_order_ix(
        &self,
        order: BookOrder,
        price: Option<u64>,
        base_asset_amount: Option<u64>,
    ) -> Instruction {
        let user = &self.users[order.owner_idx];
        let mut accounts = velocity::accounts::ModifyOrderV1 {
            state: self.state_pda(),
            user: user.user_pda,
            authority: user.keypair.pubkey(),
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
            flow_authority: None,
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(true));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::ModifyOrderV1 {
                params: ModifyOrderV1Params {
                    market_index: crate::clob::MARKET_INDEX,
                    order_ref: order.order_ref,
                    price,
                    base_asset_amount,
                    max_ts: None,
                    activation_delay_slots: None,
                    reject_if_crossed: false,
                },
            }
            .data(),
        }
    }
}

/// Keeper instructions: a filler acts on another user's orders.
impl Fixture {
    pub(crate) fn remove_expired_ix(&self, filler_idx: usize, order: BookOrder) -> Instruction {
        let filler = &self.users[filler_idx];
        let mut accounts = velocity::accounts::CrankClobOrderRemoval {
            state: self.state_pda(),
            authority: filler.keypair.pubkey(),
            filler: filler.user_pda,
            filler_stats: filler.stats_pda,
            user: self.users[order.owner_idx].user_pda,
            perp_market: self.perp_market_pda,
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
            crank_conditions: None,
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(true));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::CrankClobRemoveExpired {
                args: CrankClobRemoveExpiredArgs {
                    market_index: crate::clob::MARKET_INDEX,
                    order_ref: order.order_ref,
                },
            }
            .data(),
        }
    }

    pub(crate) fn evict_ix(
        &self,
        filler_idx: usize,
        owner_idx: usize,
        side: ClobSide,
    ) -> Instruction {
        let filler = &self.users[filler_idx];
        let mut accounts = velocity::accounts::CrankClobOrderRemoval {
            state: self.state_pda(),
            authority: filler.keypair.pubkey(),
            filler: filler.user_pda,
            filler_stats: filler.stats_pda,
            user: self.users[owner_idx].user_pda,
            perp_market: self.perp_market_pda,
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
            crank_conditions: None,
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(true));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::CrankClobEvict {
                args: CrankClobEvictArgs {
                    market_index: crate::clob::MARKET_INDEX,
                    side,
                },
            }
            .data(),
        }
    }

    /// Sweep a failing account's book orders. No refs: the sweep takes each
    /// whole side the account's position does not reduce.
    pub(crate) fn force_cancel_book_ix(&self, filler_idx: usize, target_idx: usize) -> Instruction {
        let filler = &self.users[filler_idx];
        let mut accounts = velocity::accounts::ForceCancelClobOrders {
            state: self.state_pda(),
            authority: filler.keypair.pubkey(),
            filler: filler.user_pda,
            filler_stats: filler.stats_pda,
            user: self.users[target_idx].user_pda,
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
            crank_conditions: None,
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(true));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::ForceCancelClobOrders {
                args: ForceCancelClobOrdersArgs {
                    market_index: crate::clob::MARKET_INDEX,
                    order_refs: Vec::new(),
                },
            }
            .data(),
        }
    }
}

/// Firing a trigger order that waits in a `user.orders` slot.
impl Fixture {
    /// A fired trigger-market takes at once, so it carries the whole route.
    pub(crate) fn trigger_market_ix(
        &self,
        filler_idx: usize,
        user_idx: usize,
        order_id: u32,
    ) -> Instruction {
        let filler = &self.users[filler_idx];
        let user = &self.users[user_idx];
        let mut accounts = velocity::accounts::TriggerMarketOrderV1 {
            state: self.state_pda(),
            authority: filler.keypair.pubkey(),
            filler: filler.user_pda,
            filler_stats: filler.stats_pda,
            user: user.user_pda,
            user_stats: user.stats_pda,
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
            crank_conditions: None,
            trigger_conditions: None,
            ix_sysvar: Some(instructions_sysvar_id()),
        }
        .to_account_metas(None);
        accounts.extend(self.route_remaining_accounts(user_idx));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::TriggerMarketOrderV1 {
                args: TriggerMarketOrderV1Args {
                    market_index: crate::clob::MARKET_INDEX,
                    order_id,
                    signed_route: Vec::new(),
                },
            }
            .data(),
        }
    }

    /// A fired trigger-limit rests on the book.
    pub(crate) fn trigger_limit_ix(
        &self,
        filler_idx: usize,
        user_idx: usize,
        order_id: u32,
    ) -> Instruction {
        let filler = &self.users[filler_idx];
        let user = &self.users[user_idx];
        let mut accounts = velocity::accounts::TriggerLimitOrderV1 {
            state: self.state_pda(),
            authority: filler.keypair.pubkey(),
            filler: filler.user_pda,
            filler_stats: filler.stats_pda,
            user: user.user_pda,
            user_stats: user.stats_pda,
            quoter_slab: self.clob.quoter_slab,
            clob_market: self.clob.book,
            clob_program: self.clob.program,
            crank_conditions: None,
            trigger_conditions: None,
        }
        .to_account_metas(None);
        accounts.extend(self.market_ras(true));
        Instruction {
            program_id: velocity_program_id(),
            accounts,
            data: velocity::instruction::TriggerLimitOrderV1 {
                args: TriggerLimitOrderV1Args {
                    market_index: crate::clob::MARKET_INDEX,
                    order_id,
                },
            }
            .data(),
        }
    }
}
