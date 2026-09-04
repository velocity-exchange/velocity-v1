use {
    crate::{
        error::ErrorCode,
        state::order_params::{
            OrderParams, SignedMsgOrderParamsDelegateMessage, SignedMsgOrderParamsMessage,
            SignedMsgTriggerOrderParams,
        },
    },
    anchor_lang::prelude::*,
    byteorder::{ByteOrder, LE},
    solana_program::program_memory::sol_memcmp,
    std::convert::TryInto,
};

#[cfg(test)]
mod tests;

const SIGNATURE_LEN: usize = 64;
const PUBKEY_LEN: usize = 32;
const MESSAGE_SIZE_LEN: usize = 2;

/// Start of the hex payload in the message envelope: signature, public key,
/// then the payload size.
const PAYLOAD_OFFSET: usize = SIGNATURE_LEN + PUBKEY_LEN + MESSAGE_SIZE_LEN;

#[derive(Debug)]
pub struct VerifiedMessage {
    pub signed_msg_order_params: OrderParams,
    pub sub_account_id: Option<u16>,
    pub delegate_signed_taker_pubkey: Option<Pubkey>,
    pub slot: u64,
    pub uuid: [u8; 8],
    pub take_profit_order_params: Option<SignedMsgTriggerOrderParams>,
    pub stop_loss_order_params: Option<SignedMsgTriggerOrderParams>,
    pub max_margin_ratio: Option<u16>,
    pub builder_idx: Option<u8>,
    pub builder_fee_tenth_bps: Option<u16>,
    pub isolated_position_deposit: Option<u64>,
    /// Custom quoters (PropAMMs) the taker's route names. The CLOB and vAMM
    /// baseline is implicit.
    pub route: Option<Vec<Pubkey>>,
    pub signature: [u8; 64],
}

fn slice_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && unsafe { sol_memcmp(a, b, a.len()) } == 0
}

pub fn deserialize_into_verified_message(
    payload: Vec<u8>,
    signature: &[u8; 64],
    is_delegate_signer: bool,
) -> Result<VerifiedMessage> {
    if is_delegate_signer {
        if payload.len() < 8 {
            return Err(SignatureVerificationError::InvalidMessageDataSize.into());
        }
        let min_len: usize = std::mem::size_of::<SignedMsgOrderParamsDelegateMessage>();
        let mut owned = payload;
        if owned.len() < min_len {
            owned.resize(min_len, 0);
        }
        let deserialized = SignedMsgOrderParamsDelegateMessage::deserialize(
            &mut &owned[8..], // 8 byte manual discriminator
        )
        .map_err(|_| {
            msg!("Invalid message encoding for is_delegate_signer = true");
            SignatureVerificationError::InvalidMessageDataSize
        })?;

        validate_signed_msg_network(deserialized.network)?;
        Ok(VerifiedMessage {
            signed_msg_order_params: deserialized.signed_msg_order_params,
            sub_account_id: None,
            delegate_signed_taker_pubkey: Some(deserialized.taker_pubkey),
            slot: deserialized.slot,
            uuid: deserialized.uuid,
            take_profit_order_params: deserialized.take_profit_order_params,
            stop_loss_order_params: deserialized.stop_loss_order_params,
            max_margin_ratio: deserialized.max_margin_ratio,
            builder_idx: deserialized.builder_idx,
            builder_fee_tenth_bps: deserialized.builder_fee_tenth_bps,
            isolated_position_deposit: deserialized.isolated_position_deposit,
            route: validate_signed_msg_route(deserialized.route)?,
            signature: *signature,
        })
    } else {
        if payload.len() < 8 {
            return Err(SignatureVerificationError::InvalidMessageDataSize.into());
        }
        let min_len: usize = std::mem::size_of::<SignedMsgOrderParamsMessage>();
        let mut owned = payload;
        if owned.len() < min_len {
            owned.resize(min_len, 0);
        }
        let deserialized = SignedMsgOrderParamsMessage::deserialize(
            &mut &owned[8..], // 8 byte manual discriminator
        )
        .map_err(|_| {
            msg!("Invalid delegate message encoding for with is_delegate_signer = false");
            SignatureVerificationError::InvalidMessageDataSize
        })?;
        validate_signed_msg_network(deserialized.network)?;
        Ok(VerifiedMessage {
            signed_msg_order_params: deserialized.signed_msg_order_params,
            sub_account_id: Some(deserialized.sub_account_id),
            delegate_signed_taker_pubkey: None,
            slot: deserialized.slot,
            uuid: deserialized.uuid,
            take_profit_order_params: deserialized.take_profit_order_params,
            stop_loss_order_params: deserialized.stop_loss_order_params,
            max_margin_ratio: deserialized.max_margin_ratio,
            builder_idx: deserialized.builder_idx,
            builder_fee_tenth_bps: deserialized.builder_fee_tenth_bps,
            isolated_position_deposit: deserialized.isolated_position_deposit,
            route: validate_signed_msg_route(deserialized.route)?,
            signature: *signature,
        })
    }
}

/// Reject a message signed for a different cluster.
///
/// The signature covers the order, not the chain, so without this a devnet
/// order replays verbatim against mainnet. Absent (`None`) is the pre-tag
/// encoding and passes — nothing is deployed to mainnet yet, and the
/// verifier zero-pads short payloads, so old producers keep working until
/// they emit the tag.
/// Refuse an over-long route rather than truncating it: a taker's signed
/// route is a statement about where their order may fill, and silently
/// dropping entries would fill somewhere they did not sign for.
fn validate_signed_msg_route(
    route: Option<Vec<Pubkey>>,
) -> std::result::Result<Option<Vec<Pubkey>>, anchor_lang::error::Error> {
    if let Some(entries) = &route {
        if entries.len() > crate::state::order_params::MAX_SIGNED_MSG_ROUTE_LEN {
            msg!(
                "signed route names {} quoters, max {}",
                entries.len(),
                crate::state::order_params::MAX_SIGNED_MSG_ROUTE_LEN
            );
            return Err(SignatureVerificationError::InvalidMessageDataSize.into());
        }
    }
    Ok(route)
}

fn validate_signed_msg_network(
    network: Option<u8>,
) -> std::result::Result<(), anchor_lang::error::Error> {
    let expected = crate::state::order_params::expected_signed_msg_network();
    match network {
        Some(tag) if tag != expected => {
            msg!(
                "signed message is for network {} but this program is {}",
                tag as char,
                expected as char
            );
            Err(SignatureVerificationError::InvalidMessageDataSize.into())
        }
        _ => Ok(()),
    }
}

/// A flow attestation: the flow authority's detached signature over one
/// order's own signature plus an expiry. It marks the order's flow as
/// having served the swift hold, without the flow authority signing the
/// transaction. A transaction signer is transaction-global — the fill
/// transaction is keeper-built, and a co-signature on it needed an
/// allowlist, a shape proof, and a drain-vector analysis. A detached
/// signature over one order's signature authorizes exactly one thing.
/// It also costs no signature fee: it is verified in-program, like the
/// taker signature it binds to.
#[derive(
    anchor_lang::prelude::AnchorSerialize,
    anchor_lang::prelude::AnchorDeserialize,
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
)]
pub struct FlowAttestationV0 {
    pub signature: [u8; 64],
    /// Unix seconds this attestation is good until. Bounds how long a
    /// keeper can sit on a released attestation before filling.
    pub expiry_ts: i64,
}

/// Domain separator for [`FlowAttestationV0`] messages, so a flow-authority
/// signature made for this purpose verifies for no other.
pub const FLOW_ATTESTATION_DOMAIN: &[u8] = b"velocity.flow.attestation.v0";

/// Verify a flow attestation against the current flow authority, the order
/// signature it must bind to, and the clock. An invalid, expired, or
/// impossible (no flow authority configured) attestation is an error rather
/// than an unattested fill: the caller claimed protection it does not have.
pub fn verify_flow_attestation(
    attestation: &FlowAttestationV0,
    flow_authority: &anchor_lang::prelude::Pubkey,
    order_signature: &[u8; 64],
    now: i64,
) -> Result<()> {
    if *flow_authority == anchor_lang::prelude::Pubkey::default() {
        msg!("no flow authority is configured; attestation impossible");
        return Err(ErrorCode::SigVerificationFailed.into());
    }
    if now > attestation.expiry_ts {
        msg!(
            "flow attestation expired at {}, now {}",
            attestation.expiry_ts,
            now
        );
        return Err(ErrorCode::SigVerificationFailed.into());
    }
    brine_ed25519::verify(
        &brine_ed25519::Address::new_from_array(flow_authority.to_bytes()),
        &attestation.signature,
        &[
            FLOW_ATTESTATION_DOMAIN,
            order_signature,
            &attestation.expiry_ts.to_le_bytes(),
        ],
    )
    .map_err(|_| {
        msg!("flow attestation signature does not verify");
        ErrorCode::SigVerificationFailed.into()
    })
}

/// Verify a swift order message in-program and decode it.
///
/// `message_bytes` is the `place_signed_msg_taker_order` argument, packed as
/// `[signature: 64][public key: 32][payload size: 2 LE][payload]`. The payload
/// is the hex text the taker signed. `signer` is the taker's on-chain
/// authority (or delegate) the public key must equal.
///
/// The signature is checked over the payload with `brine_ed25519::verify`,
/// which uses the curve25519 syscalls and rejects the eight small-order
/// points. That matches the native ed25519 precompile this replaced, so no
/// sibling instruction and no instructions sysvar are needed.
pub fn verify_and_decode_signed_msg(
    message_bytes: &[u8],
    signer: &[u8; 32],
    is_delegate_signer: bool,
) -> Result<VerifiedMessage> {
    if message_bytes.len() < PAYLOAD_OFFSET {
        return Err(SignatureVerificationError::InvalidMessageDataSize.into());
    }
    let signature: [u8; 64] = message_bytes[..SIGNATURE_LEN].try_into().unwrap();
    let public_key: [u8; 32] = message_bytes[SIGNATURE_LEN..SIGNATURE_LEN + PUBKEY_LEN]
        .try_into()
        .unwrap();
    let payload_size = usize::from(LE::read_u16(
        &message_bytes[SIGNATURE_LEN + PUBKEY_LEN..PAYLOAD_OFFSET],
    ));
    let payload_end = PAYLOAD_OFFSET
        .checked_add(payload_size)
        .ok_or(SignatureVerificationError::MessageOffsetOverflow)?;
    let payload = message_bytes
        .get(PAYLOAD_OFFSET..payload_end)
        .ok_or(SignatureVerificationError::InvalidMessageDataSize)?;

    // Bind the key to the taker before the crypto: a valid signature under a
    // key that is not the taker's authority is still refused.
    if !slice_eq(&public_key, signer) {
        msg!(
            "signed message key {:?} is not the expected signer {:?}",
            public_key,
            signer
        );
        return Err(ErrorCode::SigVerificationFailed.into());
    }

    brine_ed25519::verify(
        &brine_ed25519::Address::new_from_array(public_key),
        &signature,
        &[payload],
    )
    .map_err(|_| ErrorCode::SigVerificationFailed)?;

    let payload =
        hex::decode(payload).map_err(|_| SignatureVerificationError::InvalidMessageHex)?;

    deserialize_into_verified_message(payload, &signature, is_delegate_signer)
}

// NOTE: Not `#[error_code]`. Anchor 1.0 only allows one `#[error_code]` enum
// per program in the IDL builder, and that slot is taken by `error::ErrorCode`.
// The impls below mirror what `#[error_code]` generated on anchor 0.32,
// preserving the same error_code_number offset (ERROR_CODE_OFFSET = 6000).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignatureVerificationError {
    InvalidMessageDataSize,
    MessageOffsetOverflow,
    InvalidMessageHex,
}

impl SignatureVerificationError {
    pub fn name(&self) -> &'static str {
        match self {
            Self::InvalidMessageDataSize => "InvalidMessageDataSize",
            Self::MessageOffsetOverflow => "MessageOffsetOverflow",
            Self::InvalidMessageHex => "InvalidMessageHex",
        }
    }

    pub fn msg(&self) -> &'static str {
        match self {
            Self::InvalidMessageDataSize => "invalid message data size",
            Self::MessageOffsetOverflow => "message offset overflow",
            Self::InvalidMessageHex => "invalid message hex",
        }
    }
}

impl From<SignatureVerificationError> for u32 {
    fn from(e: SignatureVerificationError) -> u32 {
        e as u32 + anchor_lang::error::ERROR_CODE_OFFSET
    }
}

impl From<SignatureVerificationError> for anchor_lang::error::Error {
    fn from(e: SignatureVerificationError) -> Self {
        anchor_lang::error::Error::from(anchor_lang::error::AnchorError {
            error_name: e.name().to_string(),
            error_code_number: e.into(),
            error_msg: e.msg().to_string(),
            error_origin: None,
            compared_values: None,
        })
    }
}
