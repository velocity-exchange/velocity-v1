//! Off-chain replay of
//! `velocity_rs::program::controller::orders::create_detached_perp_order`.
//!
//! Mirrors the simulation that the on-chain `place_signed_msg_taker_order`
//! ix runs for its main perp leg. The signature verification, slot freshness
//! check, signed-msg dedup, and SL/TP/isolated-deposit side-effects are
//! handled by the swift caller before we get here.
//!
//! The entry order is detached: it never enters `user.orders`. Replaying the
//! slot path instead would reject an order the chain accepts, because a full
//! order list fails `next_order_slot`, and it would measure margin against a
//! committed reservation rather than a modelled one.

use {
    anchor_lang::AccountDeserialize,
    solana_clock::Clock,
    std::time::{SystemTime, UNIX_EPOCH},
    velocity_rs::program::{
        controller::orders::{create_detached_perp_order, expire_orders},
        error::{ErrorCode, VelocityResult},
        instructions::optional_accounts::AccountMaps,
        sdk::{build_infos, AlignedAccountData, VelocityAccounts},
        state::{
            oracle_map::OracleMap,
            order_params::{OrderParams, PlaceOrderOptions},
            perp_market_map::PerpMarketMap,
            spot_market_map::SpotMarketMap,
            state::State as NativeState,
            user::User,
        },
    },
};

/// Off-chain replay of `create_detached_perp_order`.
///
/// `signing_slot` is the slot the taker signed at. The chain backdates the
/// order to it and prices the auction from it. A caller that holds only the
/// current slot passes that, which is what the chain resolves to anyway.
///
/// `user` is cloned before the call so the caller's value is not mutated,
/// matching the pre-FFI-removal behavior. `state_bytes` is the raw cached
/// state-account bytes (including 8-byte discriminator).
///
/// `State` is a `#[account(zero_copy)]` struct (embeds `FeeStructure` /
/// `OracleGuardRails`, which hold `u128`/`i128`), so off-chain on x86_64 it is
/// 16-aligned and its `try_deserialize` casts the body **by reference**
/// (`bytemuck::from_bytes(&data[8..])`). The raw `state_bytes` arrive in a plain
/// allocation that's 16-aligned only at the *base*, so `&data[8..]` sits at
/// `8 mod 16` and the cast panics (`TargetAlignmentGreaterAndInputNotAligned`).
/// Copy once into an [`AlignedAccountData`] buffer (body at `base + 16`) so the
/// cast lands on a 16-byte boundary — the same treatment the market/oracle
/// accounts get in `AccountsListBuilder`. The copy is trimmed to
/// `8 + size_of::<NativeState>()` first. `bytemuck::from_bytes` also panics on
/// a size mismatch. A program upgrade can extend the account past the
/// compiled-in struct, and the untrimmed copy would then panic here.
pub fn simulate_detached_perp_order(
    user: &User,
    accounts: &mut VelocityAccounts,
    state_bytes: &[u8],
    order_params: OrderParams,
    signing_slot: u64,
    max_margin_ratio: Option<u16>,
) -> VelocityResult<()> {
    let state_len = 8 + std::mem::size_of::<NativeState>();
    if state_bytes.len() < state_len {
        return Err(ErrorCode::UnableToLoadAccountLoader);
    }
    let state_aligned = AlignedAccountData::from_bytes(&state_bytes[..state_len]);
    let state = NativeState::try_deserialize(&mut state_aligned.as_slice())
        .map_err(|_| ErrorCode::UnableToLoadAccountLoader)?;

    let mut user = user.clone();
    if let Some(max_margin_ratio) = max_margin_ratio {
        user.update_perp_position_max_margin_ratio(order_params.market_index, max_margin_ratio)?;
    }

    let spot_infos = build_infos(&mut accounts.spot_markets);
    let spot_map = SpotMarketMap::load(&Default::default(), &mut spot_infos.iter().peekable())?;

    let perp_infos = build_infos(&mut accounts.perp_markets);
    let perp_map = PerpMarketMap::load(&Default::default(), &mut perp_infos.iter().peekable())?;

    let oracle_infos = build_infos(&mut accounts.oracles);
    let mut oracle_map = OracleMap::load(
        &mut oracle_infos.iter().peekable(),
        accounts.latest_slot,
        accounts.slot_clock,
        accounts.oracle_guard_rails,
    )?;

    // No epoch info — the placement only reads `slot` and `unix_timestamp`.
    let unix_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ErrorCode::UnableToCastUnixTime)?
        .as_secs() as i64;
    let local_clock = Clock {
        slot: accounts.latest_slot,
        epoch_start_timestamp: 0,
        epoch: 0,
        leader_schedule_epoch: 0,
        unix_timestamp,
    };

    let user_key = user.authority;
    let mut rev_share_order = None;
    let mut maps = AccountMaps::new(perp_map, spot_map, oracle_map);

    // `create_detached_perp_order` never touches `user.orders`, so the caller
    // owns the sweep, on chain and here. An expired order still holds its
    // reservation, and releasing it can be what lets this order pass margin.
    expire_orders(
        &mut user,
        &user_key,
        &mut maps,
        local_clock.unix_timestamp,
        local_clock.slot,
    )?;

    // A soft skip is not a rejection. An expired `max_ts` or a `TryPostOnly`
    // that would cross returns `None` and the chain's transaction still
    // succeeds, so the gate admits it.
    create_detached_perp_order(
        &state,
        &mut user,
        user_key,
        &mut maps,
        &local_clock,
        order_params,
        PlaceOrderOptions {
            enforce_margin_check: true,
            signed_msg_taker_order_slot: Some(signing_slot),
            ..PlaceOrderOptions::default()
        },
        &mut rev_share_order,
    )?;
    Ok(())
}
