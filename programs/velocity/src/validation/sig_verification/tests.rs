mod sig_verification {
    use {
        crate::{
            controller::position::PositionDirection,
            state::{
                order_params::{
                    expected_signed_msg_network, OrderParams, SignedMsgOrderParamsDelegateMessage,
                    SignedMsgOrderParamsMessage, SignedMsgTriggerOrderParams,
                },
                user::{MarketType, OrderType},
            },
            validation::sig_verification::{
                deserialize_into_verified_message, verify_and_decode_signed_msg,
            },
        },
        anchor_lang::{prelude::Pubkey, AnchorSerialize},
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

    /// The eight bytes the verifier skips before the message body.
    const MESSAGE_DISCRIMINATOR: [u8; 8] = [200, 213, 166, 94, 34, 52, 245, 93];

    /// The order every fixture carries, unless the test changes it.
    fn fixture_order_params() -> OrderParams {
        OrderParams {
            order_type: OrderType::Market,
            market_type: MarketType::Perp,
            direction: PositionDirection::Long,
            user_order_id: 1,
            base_asset_amount: 1_000_000_000,
            price: 224_000_000,
            ..OrderParams::default()
        }
    }

    fn tpsl(trigger_price: u64, base_asset_amount: u64) -> Option<SignedMsgTriggerOrderParams> {
        Some(SignedMsgTriggerOrderParams {
            trigger_price,
            base_asset_amount,
        })
    }

    /// A payload built from the message rather than from bytes. A layout change
    /// then moves the fixture with it, instead of leaving it describing
    /// something the program no longer reads.
    fn non_delegate_payload(build: impl FnOnce(&mut SignedMsgOrderParamsMessage)) -> Vec<u8> {
        let mut message = SignedMsgOrderParamsMessage {
            signed_msg_order_params: fixture_order_params(),
            sub_account_id: 0,
            slot: 1000,
            uuid: *b"Hp6TjS0k",
            network: Some(expected_signed_msg_network()),
            ..SignedMsgOrderParamsMessage::default()
        };
        build(&mut message);

        let mut payload = MESSAGE_DISCRIMINATOR.to_vec();
        message.serialize(&mut payload).unwrap();
        payload
    }

    /// [`non_delegate_payload`] for a message a delegate signed.
    fn delegate_payload(build: impl FnOnce(&mut SignedMsgOrderParamsDelegateMessage)) -> Vec<u8> {
        let mut message = SignedMsgOrderParamsDelegateMessage {
            signed_msg_order_params: OrderParams {
                direction: PositionDirection::Short,
                ..fixture_order_params()
            },
            taker_pubkey: Pubkey::default(),
            slot: 2345,
            uuid: *b"CRO3irG1",
            network: Some(expected_signed_msg_network()),
            ..SignedMsgOrderParamsDelegateMessage::default()
        };
        build(&mut message);

        let mut payload = MESSAGE_DISCRIMINATOR.to_vec();
        message.serialize(&mut payload).unwrap();
        payload
    }

    /// The in-program verifier accepts a real signature over the hex payload,
    /// and refuses a wrong signer or a tampered signature — the checks the
    /// native ed25519 precompile used to make.
    #[test]
    fn verify_and_decode_signed_msg_checks_a_real_signature() {
        use ed25519_dalek::{Signer, SigningKey};

        // A valid non-delegate order, hex-encoded: the taker signs the hex.
        let order = non_delegate_payload(|_| {});
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

    /// The all-zero key is a point of order four. `R = sB - kA` then verifies
    /// under the cofactorless equation whenever the challenge is `k` modulo
    /// four, so a forger needs about four tries and no private key.
    #[test]
    fn a_signature_forged_under_the_zero_key_is_refused() {
        use curve25519_dalek::{
            constants::ED25519_BASEPOINT_POINT, edwards::CompressedEdwardsY, scalar::Scalar,
        };

        let zero_key = [0u8; 32];
        let zero_key_point = CompressedEdwardsY(zero_key).decompress().unwrap();
        let hex_payload = hex::encode(non_delegate_payload(|_| {})).into_bytes();
        let accepted_by_cofactorless_verify = |signature: &[u8; 64]| {
            brine_ed25519::verify(
                &brine_ed25519::Address::new_from_array(zero_key),
                signature,
                &[&hex_payload],
            )
            .is_ok()
        };

        let forged = (1..64u64)
            .flat_map(|s| (0..4u64).map(move |k| (s, k)))
            .map(|(s, k)| {
                let s = Scalar::from(s);
                let r = ED25519_BASEPOINT_POINT * s - zero_key_point * Scalar::from(k);
                let mut signature = [0u8; 64];
                signature[..32].copy_from_slice(r.compress().as_bytes());
                signature[32..].copy_from_slice(s.as_bytes());
                signature
            })
            .find(|signature| accepted_by_cofactorless_verify(signature))
            .expect("a forgery under the zero key");

        let message = pack_message(&forged, &zero_key, &hex_payload);
        let err = verify_and_decode_signed_msg(&message, &zero_key, false).unwrap_err();
        assert_eq!(
            err,
            anchor_lang::error::Error::from(crate::error::ErrorCode::SigVerificationFailed)
        );
    }

    #[test]
    fn test_deserialize_into_verified_message_non_delegate() {
        let signature = [1u8; 64];
        let payload = non_delegate_payload(|_| {});

        // Test deserialization with non-delegate signer
        let result = deserialize_into_verified_message(payload, &signature, false);
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
    }

    #[test]
    fn test_deserialize_into_verified_message_non_delegate_with_tpsl() {
        let signature = [1u8; 64];
        let payload = non_delegate_payload(|m| {
            m.sub_account_id = 2;
            m.slot = 2345;
            m.uuid = *b"CRO3irG1";
            m.signed_msg_order_params.user_order_id = 3;
            m.signed_msg_order_params.base_asset_amount = 3_456_000_000;
            m.signed_msg_order_params.price = 237_000_000;
            m.take_profit_order_params = tpsl(240_000_000, 3_456_000_000);
            m.stop_loss_order_params = tpsl(225_000_000, 3_456_000_000);
        });

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(payload, &signature, false);
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
    }

    #[test]
    fn test_deserialize_into_verified_message_non_delegate_with_max_margin_ratio() {
        let signature = [1u8; 64];
        let payload = non_delegate_payload(|m| {
            m.sub_account_id = 2;
            m.slot = 2345;
            m.uuid = *b"CRO3irG1";
            m.signed_msg_order_params.user_order_id = 3;
            m.signed_msg_order_params.base_asset_amount = 3_456_000_000;
            m.signed_msg_order_params.price = 237_000_000;
            m.take_profit_order_params = tpsl(240_000_000, 3_456_000_000);
            m.stop_loss_order_params = tpsl(225_000_000, 3_456_000_000);
            m.max_margin_ratio = Some(1);
        });

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(payload, &signature, false);
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
    }

    #[test]
    fn test_deserialize_into_verified_message_non_delegate_with_isolated_position_deposit() {
        let signature = [1u8; 64];
        let payload = non_delegate_payload(|m| {
            m.sub_account_id = 2;
            m.slot = 2345;
            m.uuid = *b"CRO3irG1";
            m.signed_msg_order_params.user_order_id = 3;
            m.signed_msg_order_params.base_asset_amount = 3_456_000_000;
            m.signed_msg_order_params.price = 237_000_000;
            m.take_profit_order_params = tpsl(240_000_000, 3_456_000_000);
            m.stop_loss_order_params = tpsl(225_000_000, 3_456_000_000);
            m.isolated_position_deposit = Some(1);
        });

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(payload, &signature, false);
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
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate() {
        let signature = [1u8; 64];
        let payload = delegate_payload(|m| {
            m.taker_pubkey =
                Pubkey::from_str("HLr2UfL422cakKkaBG4z1bMZrcyhmzX2pHdegjM6fYXB").unwrap();
            m.signed_msg_order_params.user_order_id = 2;
            m.signed_msg_order_params.price = 237_000_000;
        });

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(payload, &signature, true);
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
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate_with_tpsl() {
        let signature = [1u8; 64];
        let payload = delegate_payload(|m| {
            m.taker_pubkey =
                Pubkey::from_str("HG2iQKnRkkasrLptwMZewV6wT7KPstw9wkA8yyu8Nx3m").unwrap();
            m.signed_msg_order_params.user_order_id = 2;
            m.signed_msg_order_params.price = 237_000_000;
            m.take_profit_order_params = tpsl(230_000_000, 1_000_000_000);
            m.stop_loss_order_params = tpsl(250_000_000, 1_000_000_000);
        });

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(payload, &signature, true);
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
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate_with_max_margin_ratio() {
        let signature = [1u8; 64];
        let payload = delegate_payload(|m| {
            m.taker_pubkey =
                Pubkey::from_str("HG2iQKnRkkasrLptwMZewV6wT7KPstw9wkA8yyu8Nx3m").unwrap();
            m.signed_msg_order_params.user_order_id = 2;
            m.signed_msg_order_params.price = 237_000_000;
            m.take_profit_order_params = tpsl(230_000_000, 1_000_000_000);
            m.stop_loss_order_params = tpsl(250_000_000, 1_000_000_000);
            m.max_margin_ratio = Some(1);
        });

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(payload, &signature, true);
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
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate_with_max_margin_ratio_and_builder_params() {
        let signature = [1u8; 64];
        let payload = non_delegate_payload(|m| {
            m.sub_account_id = 2;
            m.slot = 2345;
            m.uuid = *b"CRO3irG1";
            m.signed_msg_order_params.user_order_id = 3;
            m.signed_msg_order_params.base_asset_amount = 3_456_000_000;
            m.signed_msg_order_params.price = 237_000_000;
            m.take_profit_order_params = tpsl(240_000_000, 3_456_000_000);
            m.max_margin_ratio = Some(65535);
            m.builder_idx = Some(1);
            m.builder_fee_tenth_bps = Some(58);
        });

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(payload, &signature, false);
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
    }

    #[test]
    fn test_deserialize_into_verified_message_delegate_with_isolated_position_deposit() {
        let signature = [1u8; 64];
        let payload = delegate_payload(|m| {
            m.taker_pubkey =
                Pubkey::from_str("HG2iQKnRkkasrLptwMZewV6wT7KPstw9wkA8yyu8Nx3m").unwrap();
            m.signed_msg_order_params.user_order_id = 2;
            m.signed_msg_order_params.price = 237_000_000;
            m.take_profit_order_params = tpsl(230_000_000, 1_000_000_000);
            m.stop_loss_order_params = tpsl(250_000_000, 1_000_000_000);
            m.isolated_position_deposit = Some(1);
        });

        // Test deserialization with delegate signer
        let result = deserialize_into_verified_message(payload, &signature, true);
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
    }

    /// The network tag and the signed route: a message tagged for this build
    /// passes with its route intact, and one tagged for the other cluster or
    /// tagged for none at all is refused. That is the whole reason the byte
    /// exists, since the signature covers the order and not the chain.
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

        // Untagged: refused. An untagged message replays from the other
        // cluster exactly as a wrongly tagged one does.
        assert!(
            deserialize_into_verified_message(encode(None, None), &signature, false).is_err(),
            "a message that names no cluster must be refused"
        );

        // Tagged for this build, with a route: accepted verbatim. The CLOB
        // and vAMM baseline is implicit, so only the custom quoter is named.
        let tagged = deserialize_into_verified_message(
            encode(Some(expected_signed_msg_network()), Some(vec![quoter])),
            &signature,
            false,
        )
        .expect("correctly tagged message decodes");
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
