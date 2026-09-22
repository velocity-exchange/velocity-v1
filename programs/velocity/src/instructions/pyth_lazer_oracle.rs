use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{casting::Cast, safe_math::SafeMath},
        state::pyth_lazer_oracle::{
            PythLazerOracle, PYTH_LAZER_MAX_FUTURE_SECONDS, PYTH_LAZER_MAX_STALENESS_SECONDS,
            PYTH_LAZER_ORACLE_SEED, PYTH_LAZER_STORAGE_ID,
        },
        validate,
    },
    anchor_lang::prelude::*,
    pyth_lazer::{
        message::SolanaMessage,
        payload::{PayloadData, PayloadPropertyValue},
        price::Price,
        signature,
        storage::Storage,
    },
    solana_program::sysvar::instructions::load_current_index_checked,
};

pub fn handle_update_pyth_lazer_oracle<'c: 'info, 'info>(
    ctx: Context<'info, UpdatePythLazerOracle>,
    pyth_message: Vec<u8>,
) -> Result<()> {
    let ix_idx = load_current_index_checked(&ctx.accounts.ix_sysvar.to_account_info())?;
    validate!(
        ix_idx > 0,
        ErrorCode::InvalidVerificationIxIndex,
        "instruction index must be greater than 0 to include the sig verify ix"
    )?;

    let remaining_accounts = ctx.remaining_accounts;
    let storage_account_data = ctx.accounts.pyth_lazer_storage.try_borrow_data()?;
    let pyth_storage = Storage::try_deserialize(&mut &storage_account_data[..])?;

    signature::verify_message(
        &pyth_storage,
        &ctx.accounts.ix_sysvar,
        &pyth_message,
        ix_idx - 1,
        0,
    )
    .map_err(|err| {
        msg!("signature verification error: {:?}", err);
        err
    })?;

    let deserialized_pyth_message = SolanaMessage::deserialize_slice(&pyth_message)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    let data = PayloadData::deserialize_slice_le(&deserialized_pyth_message.payload)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    validate!(
        remaining_accounts.len() == data.feeds.len(),
        ErrorCode::OracleMismatchedVaaAndPriceUpdates
    )?;

    for (account, payload_data) in remaining_accounts.iter().zip(data.feeds.iter()) {
        let pyth_lazer_oracle_loader: AccountLoader<PythLazerOracle> =
            AccountLoader::try_from(account)?;
        let mut pyth_lazer_oracle = pyth_lazer_oracle_loader.load_mut()?;

        let feed_id = payload_data.feed_id.0;

        let pda = Pubkey::find_program_address(
            &[PYTH_LAZER_ORACLE_SEED, &feed_id.to_le_bytes()],
            &crate::ID,
        )
        .0;
        require_keys_eq!(
            *account.key,
            pda,
            ErrorCode::OracleBadRemainingAccountPublicKey
        );

        let current_timestamp = pyth_lazer_oracle.publish_time;

        let PayloadPropertyValue::Price(Some(price)) = payload_data.properties[0] else {
            return Err(ErrorCode::InvalidPythLazerMessage.into());
        };

        let mut best_bid_price: Option<Price> = None;
        let mut best_ask_price: Option<Price> = None;
        let mut exponent: Option<i16> = None;
        let mut next_timestamp: Option<u64> = None;
        let mut signed_confidence: Option<Price> = None;

        for property in &payload_data.properties {
            match property {
                PayloadPropertyValue::BestBidPrice(price) => best_bid_price = *price,
                PayloadPropertyValue::BestAskPrice(price) => best_ask_price = *price,
                PayloadPropertyValue::Exponent(exp) => exponent = Some(*exp),
                PayloadPropertyValue::Confidence(confidence) => signed_confidence = *confidence,
                PayloadPropertyValue::FeedUpdateTimestamp(timestamp) => match timestamp {
                    Some(timestamp) => next_timestamp = Some(timestamp.as_micros()),
                    None => continue,
                },
                _ => {}
            }
        }

        if next_timestamp.is_none() {
            msg!("Skipping lazer price update. next_timestamp is None",);
            continue;
        }

        // The gate rejects an equal timestamp as well as an older one. A repeated
        // timestamp carries the same signed content and adds no price information,
        // but posting it would still stamp `posted_slot` fresh, letting a keeper hold
        // the feed slot-fresh every slot while the price never moves.
        if next_timestamp.unwrap() <= current_timestamp {
            msg!(
                "Skipping lazer price update. next_timestamp {} <= current_timestamp {}",
                next_timestamp.unwrap(),
                current_timestamp
            );
            continue;
        }

        // Bound the feed timestamp against the wall clock in both directions; the
        // monotonic check above only guarantees it is above the cached value, not
        // near the present. The lower bound matters because `posted_slot`, set to
        // the current slot below, is the sole input to downstream staleness, so an
        // old but authentic message would read as slot-fresh. The upper bound caps
        // how long a message stamped ahead of the clock can freeze later messages.
        let now = Clock::get()?.unix_timestamp;
        let next_timestamp_secs = next_timestamp.unwrap().safe_div(1_000_000)?.cast::<i64>()?;
        let age = now.safe_sub(next_timestamp_secs)?;
        if age > PYTH_LAZER_MAX_STALENESS_SECONDS {
            msg!(
                "Skipping lazer price update. message ts {}s is older than {}s (now {}s)",
                next_timestamp_secs,
                PYTH_LAZER_MAX_STALENESS_SECONDS,
                now
            );
            continue;
        }
        if age < -PYTH_LAZER_MAX_FUTURE_SECONDS {
            msg!(
                "Skipping lazer price update. message ts {}s is more than {}s ahead (now {}s)",
                next_timestamp_secs,
                PYTH_LAZER_MAX_FUTURE_SECONDS,
                now
            );
            continue;
        }

        let price = price.mantissa_i64();
        validate!(
            price != 0,
            ErrorCode::InvalidPythLazerMessage,
            "Pyth lazer price is zero, not enough publishers"
        )?;

        let exponent = exponent.ok_or(ErrorCode::InvalidPythLazerMessage)?;

        let conf = calculate_lazer_conf(
            price,
            best_bid_price.map(|price| price.mantissa_i64()),
            best_ask_price.map(|price| price.mantissa_i64()),
            signed_confidence.map(|price| price.mantissa_i64()),
        )?;

        pyth_lazer_oracle.price = price;
        pyth_lazer_oracle.posted_slot = Clock::get()?.slot;
        pyth_lazer_oracle.publish_time = next_timestamp.unwrap();
        pyth_lazer_oracle.exponent = exponent.cast::<i32>()?;
        pyth_lazer_oracle.conf = conf.cast::<u64>()?;
        msg!("Price updated to {}", price);

        msg!(
            "Posting new lazer update. current ts {} < next_timestamp {}",
            current_timestamp,
            next_timestamp.unwrap()
        );
    }

    Ok(())
}

/// The confidence to persist for a Lazer price update.
///
/// Confidence shares the price feed's exponent, so its mantissa compares
/// directly to `price`. The result is the widest of three signals: a 20bps
/// floor on the price, the bid-ask distance, and the signed `Confidence`
/// property, so the stored confidence never understates the message.
/// The bid-ask distance is a magnitude, since a crossed book holds the most
/// uncertainty and a signed difference would go negative there. The i128
/// subtraction saturates because two extreme mantissas overflow i64, and one
/// feed's overflow would abort every other feed in the message.
fn calculate_lazer_conf(
    price: i64,
    best_bid_price: Option<i64>,
    best_ask_price: Option<i64>,
    signed_confidence: Option<i64>,
) -> VelocityResult<i64> {
    let mut conf = price.safe_div(500)?;

    if let (Some(bid), Some(ask)) = (best_bid_price, best_ask_price) {
        let spread = i128::from(ask)
            .safe_sub(i128::from(bid))?
            .abs()
            .min(i64::MAX.into())
            .cast::<i64>()?;
        conf = conf.max(spread);
    }

    if let Some(signed_confidence) = signed_confidence {
        conf = conf.max(signed_confidence);
    }

    Ok(conf)
}

#[derive(Accounts)]
pub struct UpdatePythLazerOracle<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,
    /// CHECK: Pyth lazer storage account not available to us
    #[account(
      address = PYTH_LAZER_STORAGE_ID @ ErrorCode::InvalidPythLazerStorageOwner,
    )]
    pub pyth_lazer_storage: UncheckedAccount<'info>,
    /// CHECK: checked by ed25519 verify
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub ix_sysvar: UncheckedAccount<'info>,
}

#[cfg(test)]
mod tests {
    use super::calculate_lazer_conf;

    /// A price mantissa of 100 units at a Lazer exponent of -6.
    const PRICE: i64 = 100_000_000;
    /// The 20bps floor on `PRICE`.
    const FLOOR: i64 = PRICE / 500;

    #[test]
    fn floor_wins_when_the_other_signals_are_narrower() {
        let conf = calculate_lazer_conf(
            PRICE,
            Some(PRICE - 50_000),
            Some(PRICE + 50_000),
            Some(100_000),
        )
        .unwrap();

        assert_eq!(conf, FLOOR);
    }

    #[test]
    fn spread_wins_when_it_is_widest() {
        let conf = calculate_lazer_conf(PRICE, Some(PRICE - 300_000), Some(PRICE + 300_000), None)
            .unwrap();

        assert_eq!(conf, 600_000);
    }

    #[test]
    fn signed_confidence_wins_when_it_is_widest() {
        let conf = calculate_lazer_conf(
            PRICE,
            Some(PRICE - 50_000),
            Some(PRICE + 50_000),
            Some(900_000),
        )
        .unwrap();

        assert_eq!(conf, 900_000);
    }

    #[test]
    fn crossed_quotes_widen_the_confidence() {
        // The bid sits 500_000 above the ask. The book disagrees with itself by
        // that amount, so the confidence is 500_000, which is wider than the floor.
        let conf = calculate_lazer_conf(PRICE, Some(PRICE + 250_000), Some(PRICE - 250_000), None)
            .unwrap();

        assert_eq!(conf, 500_000);
        assert!(conf > FLOOR);
    }

    #[test]
    fn extreme_quotes_saturate_and_do_not_error() {
        // An i64 subtraction overflows here. The error would abort every other
        // feed in the same message, so the distance saturates instead.
        let conf = calculate_lazer_conf(PRICE, Some(i64::MIN), Some(i64::MAX), None).unwrap();

        assert_eq!(conf, i64::MAX);
    }
}
