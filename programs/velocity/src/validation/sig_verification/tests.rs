mod sig_verification {
    use {
        crate::{
            controller::position::PositionDirection,
            validation::sig_verification::{
                deserialize_into_verified_message, verify_and_decode_signed_msg,
            },
        },
        anchor_lang::prelude::Pubkey,
        std::str::FromStr,
    };

    /// Pack the `place_signed_msg_taker_order` argument envelope:
    /// `[signature: 64][public key: 32][payload size: 2 LE][payload]`.
    fn pack_message(signature: &[u8; 64], pubkey: &[u8; 32], payload: &[u8]) -> Vec<u8> {
        let mut message = Vec::with_capacity(98 + payload.len());
        message.extend_from_slice(signature);
        message.extend_from_slice(pubkey);
        message.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        message.extend_from_slice(payload);
        message
    }

    /// The in-program verifier accepts a real signature over the hex payload,
    /// and refuses a wrong signer or a tampered signature — the checks the
    /// native ed25519 precompile used to make.
    #[test]
    fn verify_and_decode_signed_msg_checks_a_real_signature() {
        use ed25519_dalek::{Signer, SigningKey};

        // A valid non-delegate order, hex-encoded: the taker signs the hex.
        let order = with_order_params_builder_none(vec![
            200, 213, 166, 94, 34, 52, 245, 93, 0, 1, 0, 1, 0, 202, 154, 59, 0, 0, 0, 0, 0, 248,
            89, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 192, 181, 74, 13, 0, 0, 0, 0,
            1, 0, 248, 89, 13, 0, 0, 0, 0, 0, 0, 232, 3, 0, 0, 0, 0, 0, 0, 72, 112, 54, 84, 106,
            83, 48, 107, 0, 0,
        ]);
        let hex_payload = hex::encode(&order).into_bytes();

        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let pubkey = signing.verifying_key().to_bytes();
        let signature = signing.sign(&hex_payload).to_bytes();
        let message = pack_message(&signature, &pubkey, &hex_payload);

        // The right signer: verified and decoded.
        let verified = verify_and_decode_signed_msg(&message, &pubkey, false)
            .expect("a real signature verifies");
        assert_eq!(verified.sub_account_id, Some(0));
        assert_eq!(verified.signature, signature);

        // A different expected signer: refused before the crypto.
        assert!(verify_and_decode_signed_msg(&message, &[9u8; 32], false).is_err());

        // A tampered signature: the in-program check fails.
        let mut tampered = message.clone();
        tampered[0] ^= 1;
        assert!(verify_and_decode_signed_msg(&tampered, &pubkey, false).is_err());

        // A signature by a different key, presented under that key: the pubkey
        // matches the signer but the message the taker signed differs, so it
        // is refused.
        let other = SigningKey::from_bytes(&[8u8; 32]);
        let other_pk = other.verifying_key().to_bytes();
        let other_sig = other.sign(b"not the order").to_bytes();
        let forged = pack_message(&other_sig, &other_pk, &hex_payload);
        assert!(verify_and_decode_signed_msg(&forged, &other_pk, false).is_err());
    }

    /// The fixtures below encode the embedded `OrderParams` with its pre-builder-codes layout.
    /// Builder-codes support appended two trailing `Option` fields (`builder_idx`,
    /// `builder_fee_tenth_bps`) to `OrderParams`, which in these fixtures is a 49-byte auction
    /// order starting right after the 8-byte discriminator. Splice in their `None` encodings
    /// (`0, 0`) at the end of the embedded `OrderParams` (offset 8 + 49 = 57) so the trailing
    /// envelope fields stay aligned.
    fn with_order_params_builder_none(mut payload: Vec<u8>) -> Vec<u8> {
        payload.splice(57..57, [0u8, 0u8]);
        payload
    }

    #[test]
    fn test_deserialize_into_verified_message_non_delegate() {
        let signature = [1u8; 64];
        let payload = vec![
            200, 213, 166, 94, 34, 52, 245, 93, 0, 1, 0, 1, 0, 202, 154, 59, 0, 0, 0, 0, 0, 248,
            89, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 192, 181, 74, 13, 0, 0, 0, 0,
            1, 0, 248, 89, 13, 0, 0, 0, 0, 0, 0, 232, 3, 0, 0, 0, 0, 0, 0, 72, 112, 54, 84, 106,
            83, 48, 107, 0, 0,
        ];

        // Test deserialization with non-delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            false,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, Some(0));
        assert_eq!(verified_message.delegate_signed_taker_pubkey, None);
        assert_eq!(verified_message.slot, 1000);
        assert_eq!(verified_message.uuid, [72, 112, 54, 84, 106, 83, 48, 107]);
        assert!(verified_message.take_profit_order_params.is_none());
        assert!(verified_message.stop_loss_order_params.is_none());
        assert!(verified_message.max_margin_ratio.is_none());
        assert!(verified_message.builder_idx.is_none());
        assert!(verified_message.builder_fee_tenth_bps.is_none());

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 1);
        assert_eq!(order_params.direction, PositionDirection::Long);
        assert_eq!(order_params.base_asset_amount, 1000000000u64);
        assert_eq!(order_params.price, 224000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(223000000i64));
        assert_eq!(order_params.auction_end_price, Some(224000000i64));
    }

    #[test]
    fn test_deserialize_into_verified_message_non_delegate_with_tpsl() {
        let signature = [1u8; 64];
        let payload = vec![
            200, 213, 166, 94, 34, 52, 245, 93, 0, 1, 0, 3, 0, 96, 254, 205, 0, 0, 0, 0, 64, 85,
            32, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 128, 133, 181, 13, 0, 0, 0, 0,
            1, 64, 85, 32, 14, 0, 0, 0, 0, 2, 0, 41, 9, 0, 0, 0, 0, 0, 0, 67, 82, 79, 51, 105, 114,
            71, 49, 1, 0, 28, 78, 14, 0, 0, 0, 0, 0, 96, 254, 205, 0, 0, 0, 0, 1, 64, 58, 105, 13,
            0, 0, 0, 0, 0, 96, 254, 205, 0, 0, 0, 0,
        ];

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            false,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, Some(2));
        assert_eq!(verified_message.delegate_signed_taker_pubkey, None);
        assert_eq!(verified_message.slot, 2345);
        assert_eq!(verified_message.uuid, [67, 82, 79, 51, 105, 114, 71, 49]);
        assert!(verified_message.max_margin_ratio.is_none());
        assert!(verified_message.builder_idx.is_none());
        assert!(verified_message.builder_fee_tenth_bps.is_none());

        assert!(verified_message.take_profit_order_params.is_some());
        let tp = verified_message.take_profit_order_params.unwrap();
        assert_eq!(tp.base_asset_amount, 3456000000u64);
        assert_eq!(tp.trigger_price, 240000000u64);

        assert!(verified_message.stop_loss_order_params.is_some());
        let sl = verified_message.stop_loss_order_params.unwrap();
        assert_eq!(sl.base_asset_amount, 3456000000u64);
        assert_eq!(sl.trigger_price, 225000000u64);

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 3);
        assert_eq!(order_params.direction, PositionDirection::Long);
        assert_eq!(order_params.base_asset_amount, 3456000000u64);
        assert_eq!(order_params.price, 237000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(230000000i64));
        assert_eq!(order_params.auction_end_price, Some(237000000i64));
    }

    #[test]
    fn test_deserialize_into_verified_message_non_delegate_with_max_margin_ratio() {
        let signature = [1u8; 64];
        let payload = vec![
            200, 213, 166, 94, 34, 52, 245, 93, 0, 1, 0, 3, 0, 96, 254, 205, 0, 0, 0, 0, 64, 85,
            32, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 128, 133, 181, 13, 0, 0, 0, 0,
            1, 64, 85, 32, 14, 0, 0, 0, 0, 2, 0, 41, 9, 0, 0, 0, 0, 0, 0, 67, 82, 79, 51, 105, 114,
            71, 49, 1, 0, 28, 78, 14, 0, 0, 0, 0, 0, 96, 254, 205, 0, 0, 0, 0, 1, 64, 58, 105, 13,
            0, 0, 0, 0, 0, 96, 254, 205, 0, 0, 0, 0, 1, 1,
        ];

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            false,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, Some(2));
        assert_eq!(verified_message.delegate_signed_taker_pubkey, None);
        assert_eq!(verified_message.slot, 2345);
        assert_eq!(verified_message.uuid, [67, 82, 79, 51, 105, 114, 71, 49]);
        assert!(verified_message.max_margin_ratio.is_some());
        assert_eq!(verified_message.max_margin_ratio.unwrap(), 1);
        assert!(verified_message.builder_idx.is_none());
        assert!(verified_message.builder_fee_tenth_bps.is_none());

        assert!(verified_message.take_profit_order_params.is_some());
        let tp = verified_message.take_profit_order_params.unwrap();
        assert_eq!(tp.base_asset_amount, 3456000000u64);
        assert_eq!(tp.trigger_price, 240000000u64);

        assert!(verified_message.stop_loss_order_params.is_some());
        let sl = verified_message.stop_loss_order_params.unwrap();
        assert_eq!(sl.base_asset_amount, 3456000000u64);
        assert_eq!(sl.trigger_price, 225000000u64);

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 3);
        assert_eq!(order_params.direction, PositionDirection::Long);
        assert_eq!(order_params.base_asset_amount, 3456000000u64);
        assert_eq!(order_params.price, 237000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(230000000i64));
        assert_eq!(order_params.auction_end_price, Some(237000000i64));
    }

    #[test]
    fn test_deserialize_into_verified_message_non_delegate_with_isolated_position_deposit() {
        let signature = [1u8; 64];
        let payload = vec![
            200, 213, 166, 94, 34, 52, 245, 93, 0, 1, 0, 3, 0, 96, 254, 205, 0, 0, 0, 0, 64, 85,
            32, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 128, 133, 181, 13, 0, 0, 0, 0,
            1, 64, 85, 32, 14, 0, 0, 0, 0, 2, 0, 41, 9, 0, 0, 0, 0, 0, 0, 67, 82, 79, 51, 105, 114,
            71, 49, 1, 0, 28, 78, 14, 0, 0, 0, 0, 0, 96, 254, 205, 0, 0, 0, 0, 1, 64, 58, 105, 13,
            0, 0, 0, 0, 0, 96, 254, 205, 0, 0, 0, 0, 0, 0, 0, 1, 1,
        ];

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            false,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, Some(2));
        assert_eq!(verified_message.delegate_signed_taker_pubkey, None);
        assert_eq!(verified_message.slot, 2345);
        assert_eq!(verified_message.uuid, [67, 82, 79, 51, 105, 114, 71, 49]);
        assert!(verified_message.isolated_position_deposit.is_some());
        assert_eq!(verified_message.isolated_position_deposit.unwrap(), 1);

        assert!(verified_message.take_profit_order_params.is_some());
        let tp = verified_message.take_profit_order_params.unwrap();
        assert_eq!(tp.base_asset_amount, 3456000000u64);
        assert_eq!(tp.trigger_price, 240000000u64);

        assert!(verified_message.stop_loss_order_params.is_some());
        let sl = verified_message.stop_loss_order_params.unwrap();
        assert_eq!(sl.base_asset_amount, 3456000000u64);
        assert_eq!(sl.trigger_price, 225000000u64);

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 3);
        assert_eq!(order_params.direction, PositionDirection::Long);
        assert_eq!(order_params.base_asset_amount, 3456000000u64);
        assert_eq!(order_params.price, 237000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(230000000i64));
        assert_eq!(order_params.auction_end_price, Some(237000000i64));
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate() {
        let signature = [1u8; 64];
        let payload = vec![
            66, 101, 102, 56, 199, 37, 158, 35, 0, 1, 1, 2, 0, 202, 154, 59, 0, 0, 0, 0, 64, 85,
            32, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 0, 28, 78, 14, 0, 0, 0, 0, 1,
            128, 151, 47, 14, 0, 0, 0, 0, 242, 208, 117, 159, 92, 135, 34, 224, 147, 14, 64, 92, 7,
            25, 145, 237, 79, 35, 72, 24, 140, 13, 25, 189, 134, 243, 232, 5, 89, 37, 166, 242, 41,
            9, 0, 0, 0, 0, 0, 0, 67, 82, 79, 51, 105, 114, 71, 49, 0, 0,
        ];

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            true,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, None);
        assert_eq!(
            verified_message.delegate_signed_taker_pubkey,
            Some(Pubkey::from_str("HLr2UfL422cakKkaBG4z1bMZrcyhmzX2pHdegjM6fYXB").unwrap())
        );
        assert_eq!(verified_message.slot, 2345);
        assert_eq!(verified_message.uuid, [67, 82, 79, 51, 105, 114, 71, 49]);
        assert!(verified_message.take_profit_order_params.is_none());
        assert!(verified_message.stop_loss_order_params.is_none());
        assert!(verified_message.max_margin_ratio.is_none());
        assert!(verified_message.builder_idx.is_none());
        assert!(verified_message.builder_fee_tenth_bps.is_none());

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 2);
        assert_eq!(order_params.direction, PositionDirection::Short);
        assert_eq!(order_params.base_asset_amount, 1000000000u64);
        assert_eq!(order_params.price, 237000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(240000000i64));
        assert_eq!(order_params.auction_end_price, Some(238000000i64));
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate_with_tpsl() {
        let signature = [1u8; 64];
        let payload = vec![
            66, 101, 102, 56, 199, 37, 158, 35, 0, 1, 1, 2, 0, 202, 154, 59, 0, 0, 0, 0, 64, 85,
            32, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 0, 28, 78, 14, 0, 0, 0, 0, 1,
            128, 151, 47, 14, 0, 0, 0, 0, 241, 148, 164, 10, 232, 65, 33, 157, 18, 12, 251, 132,
            245, 208, 37, 127, 112, 55, 83, 186, 54, 139, 1, 135, 220, 180, 208, 219, 189, 94, 79,
            148, 41, 9, 0, 0, 0, 0, 0, 0, 67, 82, 79, 51, 105, 114, 71, 49, 1, 128, 133, 181, 13,
            0, 0, 0, 0, 0, 202, 154, 59, 0, 0, 0, 0, 1, 128, 178, 230, 14, 0, 0, 0, 0, 0, 202, 154,
            59, 0, 0, 0, 0,
        ];

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            true,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, None);
        assert_eq!(
            verified_message.delegate_signed_taker_pubkey,
            Some(Pubkey::from_str("HG2iQKnRkkasrLptwMZewV6wT7KPstw9wkA8yyu8Nx3m").unwrap())
        );
        assert_eq!(verified_message.slot, 2345);
        assert_eq!(verified_message.uuid, [67, 82, 79, 51, 105, 114, 71, 49]);
        assert!(verified_message.max_margin_ratio.is_none());
        assert!(verified_message.builder_idx.is_none());
        assert!(verified_message.builder_fee_tenth_bps.is_none());

        assert!(verified_message.take_profit_order_params.is_some());
        let tp = verified_message.take_profit_order_params.unwrap();
        assert_eq!(tp.base_asset_amount, 1000000000u64);
        assert_eq!(tp.trigger_price, 230000000u64);

        assert!(verified_message.stop_loss_order_params.is_some());
        let sl = verified_message.stop_loss_order_params.unwrap();
        assert_eq!(sl.base_asset_amount, 1000000000u64);
        assert_eq!(sl.trigger_price, 250000000u64);

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 2);
        assert_eq!(order_params.direction, PositionDirection::Short);
        assert_eq!(order_params.base_asset_amount, 1000000000u64);
        assert_eq!(order_params.price, 237000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(240000000i64));
        assert_eq!(order_params.auction_end_price, Some(238000000i64));
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate_with_max_margin_ratio() {
        let signature = [1u8; 64];
        let payload = vec![
            66, 101, 102, 56, 199, 37, 158, 35, 0, 1, 1, 2, 0, 202, 154, 59, 0, 0, 0, 0, 64, 85,
            32, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 0, 28, 78, 14, 0, 0, 0, 0, 1,
            128, 151, 47, 14, 0, 0, 0, 0, 241, 148, 164, 10, 232, 65, 33, 157, 18, 12, 251, 132,
            245, 208, 37, 127, 112, 55, 83, 186, 54, 139, 1, 135, 220, 180, 208, 219, 189, 94, 79,
            148, 41, 9, 0, 0, 0, 0, 0, 0, 67, 82, 79, 51, 105, 114, 71, 49, 1, 128, 133, 181, 13,
            0, 0, 0, 0, 0, 202, 154, 59, 0, 0, 0, 0, 1, 128, 178, 230, 14, 0, 0, 0, 0, 0, 202, 154,
            59, 0, 0, 0, 0, 1, 1,
        ];

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            true,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, None);
        assert_eq!(
            verified_message.delegate_signed_taker_pubkey,
            Some(Pubkey::from_str("HG2iQKnRkkasrLptwMZewV6wT7KPstw9wkA8yyu8Nx3m").unwrap())
        );
        assert_eq!(verified_message.slot, 2345);
        assert_eq!(verified_message.uuid, [67, 82, 79, 51, 105, 114, 71, 49]);
        assert!(verified_message.max_margin_ratio.is_some());
        assert_eq!(verified_message.max_margin_ratio.unwrap(), 1);
        assert!(verified_message.builder_idx.is_none());
        assert!(verified_message.builder_fee_tenth_bps.is_none());

        assert!(verified_message.builder_idx.is_none());
        assert!(verified_message.builder_fee_tenth_bps.is_none());

        assert!(verified_message.take_profit_order_params.is_some());
        let tp = verified_message.take_profit_order_params.unwrap();
        assert_eq!(tp.base_asset_amount, 1000000000u64);
        assert_eq!(tp.trigger_price, 230000000u64);

        assert!(verified_message.stop_loss_order_params.is_some());
        let sl = verified_message.stop_loss_order_params.unwrap();
        assert_eq!(sl.base_asset_amount, 1000000000u64);
        assert_eq!(sl.trigger_price, 250000000u64);

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 2);
        assert_eq!(order_params.direction, PositionDirection::Short);
        assert_eq!(order_params.base_asset_amount, 1000000000u64);
        assert_eq!(order_params.price, 237000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(240000000i64));
        assert_eq!(order_params.auction_end_price, Some(238000000i64));
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate_with_max_margin_ratio_and_builder_params() {
        let signature = [1u8; 64];
        let payload = vec![
            200, 213, 166, 94, 34, 52, 245, 93, 0, 1, 0, 3, 0, 96, 254, 205, 0, 0, 0, 0, 64, 85,
            32, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 128, 133, 181, 13, 0, 0, 0, 0,
            1, 64, 85, 32, 14, 0, 0, 0, 0, 2, 0, 41, 9, 0, 0, 0, 0, 0, 0, 67, 82, 79, 51, 105, 114,
            71, 49, 1, 0, 28, 78, 14, 0, 0, 0, 0, 0, 96, 254, 205, 0, 0, 0, 0, 0, 1, 255, 255, 1,
            1, 1, 58, 0,
        ];

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            false,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, Some(2));
        assert_eq!(verified_message.delegate_signed_taker_pubkey, None);
        assert_eq!(verified_message.slot, 2345);
        assert_eq!(verified_message.uuid, [67, 82, 79, 51, 105, 114, 71, 49]);
        assert_eq!(verified_message.max_margin_ratio.unwrap(), 65535);
        assert_eq!(verified_message.builder_idx.unwrap(), 1);
        assert_eq!(verified_message.builder_fee_tenth_bps.unwrap(), 58);

        assert!(verified_message.take_profit_order_params.is_some());
        let tp = verified_message.take_profit_order_params.unwrap();
        assert_eq!(tp.base_asset_amount, 3456000000u64);
        assert_eq!(tp.trigger_price, 240000000u64);

        assert!(verified_message.stop_loss_order_params.is_none());

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 3);
        assert_eq!(order_params.direction, PositionDirection::Long);
        assert_eq!(order_params.base_asset_amount, 3456000000u64);
        assert_eq!(order_params.price, 237000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(230000000i64));
        assert_eq!(order_params.auction_end_price, Some(237000000i64));
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate_with_isolated_position_deposit() {
        let signature = [1u8; 64];
        let payload = vec![
            66, 101, 102, 56, 199, 37, 158, 35, 0, 1, 1, 2, 0, 202, 154, 59, 0, 0, 0, 0, 64, 85,
            32, 14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 0, 28, 78, 14, 0, 0, 0, 0, 1,
            128, 151, 47, 14, 0, 0, 0, 0, 241, 148, 164, 10, 232, 65, 33, 157, 18, 12, 251, 132,
            245, 208, 37, 127, 112, 55, 83, 186, 54, 139, 1, 135, 220, 180, 208, 219, 189, 94, 79,
            148, 41, 9, 0, 0, 0, 0, 0, 0, 67, 82, 79, 51, 105, 114, 71, 49, 1, 128, 133, 181, 13,
            0, 0, 0, 0, 0, 202, 154, 59, 0, 0, 0, 0, 1, 128, 178, 230, 14, 0, 0, 0, 0, 0, 202, 154,
            59, 0, 0, 0, 0, 0, 0, 0, 1, 1,
        ];

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(
            with_order_params_builder_none(payload),
            &signature,
            true,
        );
        assert!(result.is_ok());

        let verified_message = result.unwrap();

        // Verify the deserialized message has expected structure
        assert_eq!(verified_message.signature, signature);
        assert_eq!(verified_message.sub_account_id, None);
        assert_eq!(
            verified_message.delegate_signed_taker_pubkey,
            Some(Pubkey::from_str("HG2iQKnRkkasrLptwMZewV6wT7KPstw9wkA8yyu8Nx3m").unwrap())
        );
        assert_eq!(verified_message.slot, 2345);
        assert_eq!(verified_message.uuid, [67, 82, 79, 51, 105, 114, 71, 49]);
        assert!(verified_message.isolated_position_deposit.is_some());
        assert_eq!(verified_message.isolated_position_deposit.unwrap(), 1);

        assert!(verified_message.take_profit_order_params.is_some());
        let tp = verified_message.take_profit_order_params.unwrap();
        assert_eq!(tp.base_asset_amount, 1000000000u64);
        assert_eq!(tp.trigger_price, 230000000u64);

        assert!(verified_message.stop_loss_order_params.is_some());
        let sl = verified_message.stop_loss_order_params.unwrap();
        assert_eq!(sl.base_asset_amount, 1000000000u64);
        assert_eq!(sl.trigger_price, 250000000u64);

        // Verify order params
        let order_params = &verified_message.signed_msg_order_params;
        assert_eq!(order_params.user_order_id, 2);
        assert_eq!(order_params.direction, PositionDirection::Short);
        assert_eq!(order_params.base_asset_amount, 1000000000u64);
        assert_eq!(order_params.price, 237000000u64);
        assert_eq!(order_params.market_index, 0);
        assert_eq!(order_params.reduce_only, false);
        assert_eq!(order_params.auction_duration, Some(10));
        assert_eq!(order_params.auction_start_price, Some(240000000i64));
        assert_eq!(order_params.auction_end_price, Some(238000000i64));
    }

    /// The network tag and the signed route: an untagged message (every
    /// producer before the tag existed) still decodes, a message tagged for
    /// this build passes with its route intact, and one tagged for the other
    /// cluster is refused — which is the whole reason the byte exists, since
    /// the signature covers the order and not the chain.
    #[test]
    fn network_tag_is_enforced_and_the_route_round_trips() {
        use {
            crate::state::order_params::{
                expected_signed_msg_network, OrderParams, SignedMsgOrderParamsMessage,
                SIGNED_MSG_NETWORK_DEVNET, SIGNED_MSG_NETWORK_MAINNET,
            },
            anchor_lang::AnchorSerialize,
        };

        let quoter = Pubkey::from_str("BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU").unwrap();
        let encode = |network: Option<u8>, route: Option<Vec<Pubkey>>| {
            let message = SignedMsgOrderParamsMessage {
                signed_msg_order_params: OrderParams {
                    base_asset_amount: 1_000_000_000,
                    ..OrderParams::default()
                },
                sub_account_id: 0,
                slot: 42,
                uuid: *b"CRO3irG1",
                take_profit_order_params: None,
                stop_loss_order_params: None,
                max_margin_ratio: None,
                builder_idx: None,
                builder_fee_tenth_bps: None,
                isolated_position_deposit: None,
                network,
                route,
            };
            let mut payload = vec![0u8; 8]; // manual discriminator
            message.serialize(&mut payload).unwrap();
            payload
        };
        let signature = [1u8; 64];

        // Untagged: the pre-tag encoding keeps working.
        let untagged = deserialize_into_verified_message(encode(None, None), &signature, false)
            .expect("untagged message decodes");
        assert_eq!(untagged.network, None);
        assert_eq!(untagged.route, None);

        // Tagged for this build, with a route: accepted verbatim. The CLOB
        // and vAMM baseline is implicit, so only the custom quoter is named.
        let tagged = deserialize_into_verified_message(
            encode(Some(expected_signed_msg_network()), Some(vec![quoter])),
            &signature,
            false,
        )
        .expect("correctly tagged message decodes");
        assert_eq!(tagged.network, Some(expected_signed_msg_network()));
        assert_eq!(tagged.route, Some(vec![quoter]));

        // Tagged for the other cluster: refused, so a devnet order cannot
        // be replayed against mainnet state (or the reverse).
        let wrong = if expected_signed_msg_network() == SIGNED_MSG_NETWORK_DEVNET {
            SIGNED_MSG_NETWORK_MAINNET
        } else {
            SIGNED_MSG_NETWORK_DEVNET
        };
        assert!(
            deserialize_into_verified_message(encode(Some(wrong), None), &signature, false)
                .is_err(),
            "a message signed for the other cluster must be refused"
        );
    }
}
