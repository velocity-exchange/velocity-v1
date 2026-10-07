//! `/attest` — the detached flow attestation that marks an order's flow as
//! attested retail flow.
//!
//! The CLOB's activation delay is the taker protection that replaced JIT;
//! orders that arrived through swift get to skip it because swift *is* the
//! protection — the order was held (and broadcast to every subscribed
//! keeper) for the hold window before anything could execute it. The
//! attestation is the proof: after the hold, this endpoint signs the flow
//! authority's key over the order's own signature plus an expiry, and the
//! keeper passes the blob as the fill's `flow_attestation` argument.
//! Velocity verifies it in-program, next to the taker signature it binds
//! to, and forwards the fact to quoters on the wire (`taker_served_window`).
//!
//! The flow authority signs no transaction. A transaction signer is
//! transaction-global — the fill transaction is keeper-built, and a
//! co-signature on it needed an allowlist, a shape proof, and a
//! drain-vector analysis. A detached signature over one order's signature
//! authorizes exactly one thing, costs no signature fee, and needs no
//! custody of the keeper's transaction: the endpoint returns the same blob
//! to every asker, and the order still fills only once on-chain.
//!
//! A held order is keyed by the taker's order signature and not by its uuid.
//! The taker chooses the uuid, and replay protection scopes it to one taker,
//! so a second taker can submit an order with the same uuid. Keyed by uuid,
//! that order would replace the first one's entry.

use {
    axum::{extract::State, http::StatusCode, response::IntoResponse, Json},
    base64::Engine,
    dashmap::DashMap,
    serde::{Deserialize, Serialize},
    solana_keypair::Keypair,
    solana_signer::Signer,
    std::time::{SystemTime, UNIX_EPOCH},
    velocity_rs::program::FLOW_ATTESTATION_DOMAIN,
};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct HeldOrder {
    received_ms: u64,
}

pub struct AttestContext {
    /// Absent = attestation disabled (the endpoint answers 503 and the
    /// on-chain gate keeps fast activation closed anyway while the hot
    /// role is unset).
    keypair: Option<Keypair>,
    hold_ms: u64,
    expiry_ms: u64,
    held: DashMap<[u8; 64], HeldOrder>,
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
    /// receive timestamp, not this call. A second receipt of the same order
    /// keeps the first timestamp, so a resubmission cannot move the expiry.
    pub fn record(&self, order_signature: [u8; 64], received_ms: u64) {
        if self.keypair.is_none() {
            return;
        }

        // Opportunistic eviction keeps the map bounded without a sweeper.
        if self.held.len() > 4096 {
            let horizon = now_ms().saturating_sub(self.expiry_ms);
            self.held.retain(|_, held| held.received_ms >= horizon);
        }

        self.held
            .entry(order_signature)
            .or_insert(HeldOrder { received_ms });
    }
}

/// The attestation message: the domain, the order's own signature, and the
/// expiry. Public so a test can verify what this endpoint signs against
/// velocity's own verifier.
pub fn attestation_message(order_signature: &[u8; 64], expiry_ts: i64) -> Vec<u8> {
    let mut message = Vec::with_capacity(FLOW_ATTESTATION_DOMAIN.len() + order_signature.len() + 8);
    message.extend_from_slice(FLOW_ATTESTATION_DOMAIN);
    message.extend_from_slice(order_signature);
    message.extend_from_slice(&expiry_ts.to_le_bytes());
    message
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestRequest {
    /// The taker's order signature, base64, as delivered in the keeper feed.
    order_signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestResponse {
    flow_authority: String,
    /// The detached attestation signature, base64. Passed to
    /// `place_signed_msg_taker_order` as `flow_attestation.signature`.
    signature: String,
    /// Unix seconds the attestation is good until. Passed as
    /// `flow_attestation.expiry_ts`.
    expiry_ts: i64,
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
    let Some(order_signature) = decode_order_signature(&request.order_signature) else {
        return err(
            StatusCode::BAD_REQUEST,
            "orderSignature must be 64 bytes of base64",
        );
    };

    let Some(held) = ctx.held.get(&order_signature) else {
        return err(StatusCode::NOT_FOUND, "unknown order signature");
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

    let expiry_at = held.received_ms.saturating_add(ctx.expiry_ms);
    if now > expiry_at {
        return err(StatusCode::GONE, "order is past the attestation window");
    }

    // The expiry derives from the receive timestamp, not from this call, so
    // every request for the same order gets the identical blob. That makes
    // retries free: the attestation authorizes only "this order's flow
    // served the hold, until this time", and the order fills once on-chain
    // regardless of how many keepers hold the blob.
    let expiry_ts = (expiry_at / 1_000) as i64;
    let signature = keypair.sign_message(&attestation_message(&order_signature, expiry_ts));

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(AttestResponse {
                flow_authority: keypair.pubkey().to_string(),
                signature: base64::engine::general_purpose::STANDARD
                    .encode(<[u8; 64]>::from(signature)),
                expiry_ts,
            })
            .expect("serializes"),
        ),
    )
        .into_response()
}

fn decode_order_signature(encoded: &str) -> Option<[u8; 64]> {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?
        .try_into()
        .ok()
}

fn err(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        velocity_rs::program::{verify_flow_attestation, FlowAttestationV0},
    };

    fn enabled_context() -> AttestContext {
        AttestContext {
            keypair: Some(Keypair::new()),
            hold_ms: 300,
            expiry_ms: 30_000,
            held: DashMap::new(),
        }
    }

    /// Two takers can sign orders with the same uuid. Each order keeps its
    /// own entry, and a resubmission does not move the first receipt.
    #[test]
    fn an_order_is_held_by_its_own_signature() {
        let ctx = enabled_context();
        let victim = [1u8; 64];
        let attacker = [2u8; 64];

        ctx.record(victim, 1_000);
        ctx.record(attacker, 2_000);
        ctx.record(victim, 3_000);

        assert_eq!(ctx.held.len(), 2);
        assert_eq!(ctx.held.get(&victim).unwrap().received_ms, 1_000);
        assert_eq!(ctx.held.get(&attacker).unwrap().received_ms, 2_000);
    }

    #[test]
    fn an_order_signature_decodes_only_at_64_bytes() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        assert_eq!(decode_order_signature(&encoded), Some([7u8; 64]));

        let short = base64::engine::general_purpose::STANDARD.encode([7u8; 8]);
        assert_eq!(decode_order_signature(&short), None);
        assert_eq!(decode_order_signature("not base64!"), None);
    }

    /// What this endpoint signs is what velocity's verifier accepts — bound
    /// to the order signature, the key, and the expiry, and refused for any
    /// other.
    #[test]
    fn the_attestation_round_trips_through_velocitys_verifier() {
        let flow = Keypair::new();
        let order_sig = [7u8; 64];
        let expiry_ts = 1_700_000_000i64;
        let signature = flow.sign_message(&attestation_message(&order_sig, expiry_ts));
        let attestation = FlowAttestationV0 {
            signature: <[u8; 64]>::from(signature),
            expiry_ts,
        };
        let authority = anchor_lang::prelude::Pubkey::new_from_array(flow.pubkey().to_bytes());

        verify_flow_attestation(&attestation, &authority, &order_sig, expiry_ts - 5)
            .expect("verifies for the order it binds to");
        assert!(
            verify_flow_attestation(&attestation, &authority, &[8u8; 64], expiry_ts - 5).is_err(),
            "another order's signature must not verify"
        );
        assert!(
            verify_flow_attestation(&attestation, &authority, &order_sig, expiry_ts + 1).is_err(),
            "an expired attestation must not verify"
        );

        let stranger =
            anchor_lang::prelude::Pubkey::new_from_array(Keypair::new().pubkey().to_bytes());
        assert!(
            verify_flow_attestation(&attestation, &stranger, &order_sig, expiry_ts - 5).is_err(),
            "a stranger's key must not verify"
        );
    }
}
