//! `/attest` — the flow-authority co-signature that marks a transaction as
//! attested retail flow.
//!
//! The CLOB's activation delay is the taker protection that replaced JIT;
//! orders that arrived through swift get to skip it because swift *is* the
//! protection — the order was held (and broadcast to every subscribed
//! keeper) for the hold window before anything could execute it. The
//! attestation is the proof: a keeper builds its fill transaction, posts it
//! here after the hold, and swift co-signs with the flow-authority key —
//! the key registered on-chain as `State.hot_flow_authority` and checked by
//! `place_and_make_perp_order_v1`'s fast-activation gate and by quoters (the midpoint's
//! `require_attested_flow`) via instructions-sysvar introspection.
//!
//! The signature authorizes nothing by itself, but a Solana signer is
//! transaction-global — a malicious transaction could move the key's
//! lamports — so co-signing is gated hard: the flow authority must appear
//! as a *read-only, non-fee-payer* signer, every invoked program must be on
//! the allowlist (velocity, compute budget), the transaction must demonstrably
//! fill the held order (its taker signature must appear in an instruction's
//! data — the velocity fill instruction carries it), and it must not place or
//! modify a CLOB order — those carry the fast activation the co-signature
//! unlocks, and a retail fill never rests one. Same drain-vector analysis as
//! relay's payment guards.

use {
    anchor_lang::Discriminator,
    axum::{extract::State, http::StatusCode, response::IntoResponse, Json},
    base64::Engine,
    dashmap::DashMap,
    serde::{Deserialize, Serialize},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    std::time::{SystemTime, UNIX_EPOCH},
    velocity_rs::velocity_idl::instructions::{ModifyOrderV1, PlaceAndMakePerpOrderV1},
};

const COMPUTE_BUDGET_ID: &str = "ComputeBudget111111111111111111111111111111";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct HeldOrder {
    received_ms: u64,
    order_signature: [u8; 64],
}

pub struct AttestContext {
    /// Absent = attestation disabled (the endpoint answers 503 and the
    /// on-chain gate keeps fast activation closed anyway while the hot
    /// role is unset).
    keypair: Option<Keypair>,
    hold_ms: u64,
    expiry_ms: u64,
    held: DashMap<[u8; 8], HeldOrder>,
}

impl AttestContext {
    /// `FLOW_AUTHORITY_KEYPAIR` is either an inline JSON byte array or a
    /// path to one; `ATTESTATION_HOLD_MS` / `ATTESTATION_EXPIRY_MS` tune
    /// the window.
    pub fn from_env() -> Self {
        let keypair = std::env::var("FLOW_AUTHORITY_KEYPAIR")
            .ok()
            .and_then(|raw| {
                let json = if raw.trim_start().starts_with('[') {
                    raw
                } else {
                    std::fs::read_to_string(&raw).ok()?
                };
                let bytes: Vec<u8> = serde_json::from_str(&json).ok()?;
                Keypair::try_from(bytes.as_slice()).ok()
            });
        match &keypair {
            Some(kp) => {
                log::info!(target: "attest", "flow authority enabled: {}", kp.pubkey())
            }
            None => log::info!(target: "attest", "no FLOW_AUTHORITY_KEYPAIR; /attest disabled"),
        }
        Self {
            keypair,
            hold_ms: std::env::var("ATTESTATION_HOLD_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            expiry_ms: std::env::var("ATTESTATION_EXPIRY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30_000),
            held: DashMap::new(),
        }
    }

    /// Record a verified, published order as attestable. Called by the
    /// order intake on the publish path; the hold clock is the intake's
    /// receive timestamp, not this call.
    pub fn record(&self, uuid: [u8; 8], received_ms: u64, order_signature: [u8; 64]) {
        if self.keypair.is_none() {
            return;
        }
        // Opportunistic eviction keeps the map bounded without a sweeper.
        if self.held.len() > 4096 {
            let horizon = now_ms().saturating_sub(self.expiry_ms);
            self.held.retain(|_, held| held.received_ms >= horizon);
        }
        self.held.insert(
            uuid,
            HeldOrder {
                received_ms,
                order_signature,
            },
        );
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestRequest {
    /// The order's uuid, as delivered in the keeper feed.
    uuid: String,
    /// The keeper's fully built fill transaction (base64, legacy or v0),
    /// with a signature slot for the flow authority.
    transaction: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestResponse {
    /// The same transaction, co-signed by the flow authority.
    transaction: String,
    flow_authority: String,
}

pub async fn attest(
    State(state): State<&'static crate::swift_server::ServerParams>,
    Json(request): Json<AttestRequest>,
) -> impl IntoResponse {
    let ctx = state.attest();
    let Some(keypair) = &ctx.keypair else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "attestation is not enabled",
        );
    };
    let Ok(uuid): Result<[u8; 8], _> = request.uuid.as_bytes().try_into() else {
        return err(StatusCode::BAD_REQUEST, "uuid must be exactly 8 bytes");
    };
    let Some(held) = ctx.held.get(&uuid) else {
        return err(StatusCode::NOT_FOUND, "unknown order uuid");
    };

    let now = now_ms();
    let ready_at = held.received_ms.saturating_add(ctx.hold_ms);
    if now < ready_at {
        return (
            StatusCode::TOO_EARLY,
            Json(serde_json::json!({
                "error": "hold window has not elapsed",
                "retryAfterMs": ready_at - now,
            })),
        )
            .into_response();
    }
    if now > held.received_ms.saturating_add(ctx.expiry_ms) {
        return err(StatusCode::GONE, "order is past the attestation window");
    }

    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(&request.transaction) else {
        return err(StatusCode::BAD_REQUEST, "transaction is not valid base64");
    };
    let Ok(mut tx) = bincode::deserialize::<VersionedTransaction>(&raw) else {
        return err(StatusCode::BAD_REQUEST, "transaction does not deserialize");
    };

    let order_signature = held.order_signature;
    // Release the borrow before consuming the entry below.
    drop(held);

    if let Err(reason) = validate_attestable(&tx, &keypair.pubkey(), &order_signature) {
        log::warn!(target: "attest", "refused attestation: {reason}");
        return err(StatusCode::UNPROCESSABLE_ENTITY, &reason);
    }

    // One co-signature per held order. The order signature the binding checks is
    // public — swift broadcasts it — so anyone can build a transaction that
    // carries it; consuming the entry on the first successful sign stops one
    // uuid from yielding many distinct co-signed transactions across the hold
    // window. A lost response is a lost fill, not a reuse: the taker re-signs.
    if ctx.held.remove(&uuid).is_none() {
        return err(StatusCode::CONFLICT, "order was already attested");
    }

    // Partial-sign: fill only our slot, leaving the keeper's signatures
    // (present or not) untouched.
    let flow_index = tx
        .message
        .static_account_keys()
        .iter()
        .position(|key| *key == keypair.pubkey())
        .expect("validated above");
    let signature = keypair.sign_message(&tx.message.serialize());
    tx.signatures[flow_index] = signature;

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(AttestResponse {
                transaction: base64::engine::general_purpose::STANDARD
                    .encode(bincode::serialize(&tx).expect("round-trips")),
                flow_authority: keypair.pubkey().to_string(),
            })
            .expect("serializes"),
        ),
    )
        .into_response()
}

fn err(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// The whole safety argument for lending out a signature, in one place.
fn validate_attestable(
    tx: &VersionedTransaction,
    flow_authority: &Pubkey,
    order_signature: &[u8; 64],
) -> Result<(), String> {
    let message = &tx.message;
    let keys = message.static_account_keys();
    let header = message.header();

    // Our slot: a required signer, never the fee payer, and read-only —
    // writable-signer status is what makes a signature drainable.
    let index = keys
        .iter()
        .position(|key| key == flow_authority)
        .ok_or("flow authority is not among the transaction's accounts")?;
    let signers = header.num_required_signatures as usize;
    let readonly_signed_start = signers - header.num_readonly_signed_accounts as usize;
    // A well-formed transaction carries one signature slot per required signer.
    // The signing path writes tx.signatures[index]; a short vector would panic
    // there. This request is unauthenticated, so reject the mismatch here.
    if tx.signatures.len() != signers {
        return Err("signature count does not match the required signers".into());
    }
    if index == 0 {
        return Err("flow authority must not be the fee payer".into());
    }
    if index >= signers {
        return Err("flow authority is not a required signer".into());
    }
    if index < readonly_signed_start {
        return Err("flow authority must be a read-only signer".into());
    }

    // Invoked programs are always static keys; every one must be expected.
    // An unknown program given a transaction-global signer is exactly the
    // drain vector this guards.
    let velocity = velocity_rs::constants::PROGRAM_ID;
    let compute_budget: Pubkey = COMPUTE_BUDGET_ID.parse().expect("const");
    for instruction in message.instructions() {
        let program = keys
            .get(instruction.program_id_index as usize)
            .ok_or("instruction names a program outside the static keys")?;
        if *program != velocity && *program != compute_budget {
            return Err(format!("program {program} is not attestable"));
        }
        // A retail fill never places or modifies a resting CLOB order. Those
        // are the fast-activation instructions: the co-signature lets one skip
        // the speed bump. Refusing them stops a maker-run keeper from smuggling
        // its own fast placement into a transaction co-signed for the fill —
        // the order signature the binding below checks is public, so the tx is
        // otherwise the keeper's to shape.
        if *program == velocity {
            let discriminator = instruction.data.get(..8);
            if discriminator == Some(PlaceAndMakePerpOrderV1::DISCRIMINATOR)
                || discriminator == Some(ModifyOrderV1::DISCRIMINATOR)
            {
                return Err("an attested transaction cannot place or modify a CLOB order".into());
            }
        }
    }

    // The transaction must actually fill the held order: its taker signature
    // travels in the velocity fill instruction's data.
    let binds = message.instructions().iter().any(|instruction| {
        instruction
            .data
            .windows(64)
            .any(|window| window == order_signature)
    });
    if !binds {
        return Err("transaction does not contain the held order's signature".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_hash::Hash,
        solana_instruction::{AccountMeta, Instruction},
        solana_message::{v0, VersionedMessage},
        solana_signer::Signer as _,
    };

    fn fill_like_tx(
        flow: &Pubkey,
        order_signature: &[u8; 64],
        flow_writable: bool,
        program_override: Option<Pubkey>,
    ) -> VersionedTransaction {
        let payer = Keypair::new();
        let velocity = velocity_rs::constants::PROGRAM_ID;
        // The taker signature rides the velocity instruction's own data, the
        // way place_signed_msg_taker_order carries it. No ed25519 instruction.
        let mut data = vec![0u8; 8];
        data.extend_from_slice(order_signature);
        let ixs = vec![Instruction {
            program_id: program_override.unwrap_or(velocity),
            accounts: vec![if flow_writable {
                AccountMeta::new(*flow, true)
            } else {
                AccountMeta::new_readonly(*flow, true)
            }],
            data,
        }];
        let message = v0::Message::try_compile(&payer.pubkey(), &ixs, &[], Hash::default())
            .expect("compiles");
        VersionedTransaction {
            signatures: vec![Default::default(); message.header.num_required_signatures as usize],
            message: VersionedMessage::V0(message),
        }
    }

    #[test]
    fn attestable_only_when_readonly_signer_allowlisted_and_bound() {
        let flow = Keypair::new();
        let order_sig = [7u8; 64];

        let good = fill_like_tx(&flow.pubkey(), &order_sig, false, None);
        assert!(validate_attestable(&good, &flow.pubkey(), &order_sig).is_ok());

        // Writable signer: a drainable signature. Refused.
        let writable = fill_like_tx(&flow.pubkey(), &order_sig, true, None);
        assert!(validate_attestable(&writable, &flow.pubkey(), &order_sig)
            .unwrap_err()
            .contains("read-only"));

        // Unknown program with our signer in play. Refused.
        let foreign = fill_like_tx(
            &flow.pubkey(),
            &order_sig,
            false,
            Some(Pubkey::new_unique()),
        );
        assert!(validate_attestable(&foreign, &flow.pubkey(), &order_sig)
            .unwrap_err()
            .contains("not attestable"));

        // A transaction for some other order. Refused.
        assert!(validate_attestable(&good, &flow.pubkey(), &[9u8; 64])
            .unwrap_err()
            .contains("held order"));

        // Flow authority absent entirely. Refused.
        let stranger = Keypair::new();
        assert!(validate_attestable(&good, &stranger.pubkey(), &order_sig)
            .unwrap_err()
            .contains("not among"));
    }

    #[test]
    fn attested_tx_cannot_place_a_clob_order() {
        let flow = Keypair::new();
        let order_sig = [7u8; 64];
        let payer = Keypair::new();
        let velocity = velocity_rs::constants::PROGRAM_ID;
        // A place_and_make_perp_order_v1 carries the fast-activation gate, and the
        // order signature rides its data.
        let mut place_data = PlaceAndMakePerpOrderV1::DISCRIMINATOR.to_vec();
        place_data.extend_from_slice(&order_sig);
        let ixs = vec![Instruction {
            program_id: velocity,
            accounts: vec![AccountMeta::new_readonly(flow.pubkey(), true)],
            data: place_data,
        }];
        let message = v0::Message::try_compile(&payer.pubkey(), &ixs, &[], Hash::default())
            .expect("compiles");
        let tx = VersionedTransaction {
            signatures: vec![Default::default(); message.header.num_required_signatures as usize],
            message: VersionedMessage::V0(message),
        };
        assert!(validate_attestable(&tx, &flow.pubkey(), &order_sig)
            .unwrap_err()
            .contains("CLOB order"));
    }

    #[test]
    fn short_signature_vector_is_refused_not_panicked() {
        let flow = Keypair::new();
        let order_sig = [7u8; 64];
        let mut short = fill_like_tx(&flow.pubkey(), &order_sig, false, None);
        // A malformed transaction with no signature slots. Indexing the vector
        // in the signing path would panic; validation must reject it first.
        short.signatures.clear();
        assert!(validate_attestable(&short, &flow.pubkey(), &order_sig)
            .unwrap_err()
            .contains("signature count"));
    }
}
