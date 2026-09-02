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
    /// The order's uuid, as delivered in the keeper feed.
    uuid: String,
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
    let signature = keypair.sign_message(&attestation_message(&held.order_signature, expiry_ts));

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

fn err(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        velocity_rs::program::{verify_flow_attestation, FlowAttestationV0},
    };

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
