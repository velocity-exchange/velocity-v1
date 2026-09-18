//! Client for swift's `POST /attest` — the detached flow attestation that
//! marks a fill as attested retail flow.
//!
//! Quoters that gate on `require_attested_flow` (the midpoint) quote only to
//! attested flow, and place_and_make_perp_order_v1 grants faster-than-default
//! activation only to it. A fill that carries the attestation therefore
//! reaches more liquidity.
//!
//! The attestation binds to the order, not to a transaction: swift signs the
//! flow authority's key over the order's own signature and an expiry. The
//! keeper asks for it with the order uuid alone, before it builds anything,
//! and passes the blob as the fill's `flow_attestation` argument. Velocity
//! verifies it in-program, next to the taker signature it binds to. Nothing
//! about the transaction is fixed by it, so the fill still simulates, sizes
//! its compute limit, and signs on the ordinary send path.
//!
//! Attestation failing is degradation, not failure: the caller falls back to
//! an unattested fill and takes whatever quotes without the marker.

use {
    base64::Engine,
    serde::Deserialize,
    solana_pubkey::Pubkey,
    std::{str::FromStr, time::Duration},
    velocity_rs::program::FlowAttestationV0,
};

const TARGET: &str = "attest";

/// Swift's hold window is ~300ms; two retries with the server-reported
/// backoff covers it without stalling a fill indefinitely.
const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_WAIT: Duration = Duration::from_millis(1_500);

pub struct AttestClient {
    url: String,
    flow_authority: Pubkey,
    http: reqwest::Client,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttestOk {
    flow_authority: String,
    /// The detached attestation signature, base64.
    signature: String,
    /// Unix seconds the attestation is good until.
    expiry_ts: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttestErr {
    #[serde(default)]
    error: String,
    #[serde(default)]
    retry_after_ms: Option<u64>,
}

impl AttestClient {
    /// `SWIFT_HTTP_URL` names swift's HTTP base explicitly; otherwise it is
    /// derived from `SWIFT_WS_URL` (scheme swap, `/ws` suffix dropped).
    /// `flow_authority` comes from `State.hot_flow_authority` — the caller
    /// only constructs a client when it is set.
    pub fn from_env(flow_authority: Pubkey) -> Option<Self> {
        let url = std::env::var("SWIFT_HTTP_URL")
            .ok()
            .or_else(|| http_from_ws(&std::env::var("SWIFT_WS_URL").ok()?))?;
        log::info!(target: TARGET, "attestation enabled: swift={url} flow_authority={flow_authority}");
        Some(Self {
            url,
            flow_authority,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        })
    }

    /// Ask swift for the flow attestation on `uuid`. Retries through the
    /// hold window (425 plus `retryAfterMs`).
    pub async fn attest(&self, uuid: &str) -> Result<FlowAttestationV0, String> {
        let endpoint = format!("{}/attest", self.url);
        for attempt in 0..MAX_ATTEMPTS {
            let response = self
                .http
                .post(&endpoint)
                .json(&serde_json::json!({ "uuid": uuid }))
                .send()
                .await
                .map_err(|e| format!("attest request failed: {e}"))?;
            let status = response.status();
            if status.is_success() {
                let ok: AttestOk = response
                    .json()
                    .await
                    .map_err(|e| format!("attest response undecodable: {e}"))?;
                return self.decode(ok);
            }

            let err: AttestErr = response.json().await.unwrap_or(AttestErr {
                error: status.to_string(),
                retry_after_ms: None,
            });

            // 425: the hold window hasn't run — wait it out and retry.
            if status.as_u16() == 425 && attempt + 1 < MAX_ATTEMPTS {
                let wait =
                    Duration::from_millis(err.retry_after_ms.unwrap_or(150)).min(MAX_RETRY_WAIT);
                log::debug!(target: TARGET, "hold window: waiting {wait:?} (uuid={uuid})");
                tokio::time::sleep(wait).await;
                continue;
            }

            return Err(format!("attest refused ({status}): {}", err.error));
        }

        Err("attest retries exhausted".into())
    }

    /// The program verifies the attestation against `State.hot_flow_authority`,
    /// which is the key this client was built from. A blob signed by another
    /// key fails the fill on chain, so it is refused here, where the reason
    /// is still readable.
    fn decode(&self, ok: AttestOk) -> Result<FlowAttestationV0, String> {
        let flow_authority = Pubkey::from_str(&ok.flow_authority)
            .map_err(|e| format!("attest flowAuthority unreadable: {e}"))?;
        if flow_authority != self.flow_authority {
            return Err(format!(
                "attest signed by {flow_authority}, but the chain names {}",
                self.flow_authority
            ));
        }

        let signature: [u8; 64] = base64::engine::general_purpose::STANDARD
            .decode(&ok.signature)
            .map_err(|e| format!("attest signature not base64: {e}"))?
            .try_into()
            .map_err(|raw: Vec<u8>| format!("attest signature is {} bytes, not 64", raw.len()))?;
        Ok(FlowAttestationV0 {
            signature,
            expiry_ts: ok.expiry_ts,
        })
    }
}

/// `wss://swift.example.com/ws` → `https://swift.example.com`.
fn http_from_ws(ws: &str) -> Option<String> {
    Some(
        ws.replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1)
            .trim_end_matches("/ws")
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(flow_authority: Pubkey) -> AttestClient {
        AttestClient {
            url: "http://127.0.0.1:3111".to_string(),
            flow_authority,
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn http_base_derives_from_the_ws_url() {
        assert_eq!(
            http_from_ws("wss://swift.velocity.exchange/ws").as_deref(),
            Some("https://swift.velocity.exchange")
        );
        assert_eq!(
            http_from_ws("ws://127.0.0.1:3111").as_deref(),
            Some("http://127.0.0.1:3111")
        );
    }

    #[test]
    fn a_response_decodes_into_the_attestation_the_fill_carries() {
        let flow_authority = Pubkey::new_unique();
        let attestation = client(flow_authority)
            .decode(AttestOk {
                flow_authority: flow_authority.to_string(),
                signature: base64::engine::general_purpose::STANDARD.encode([7u8; 64]),
                expiry_ts: 1_700_000_000,
            })
            .expect("decodes");
        assert_eq!(attestation.signature, [7u8; 64]);
        assert_eq!(attestation.expiry_ts, 1_700_000_000);
    }

    #[test]
    fn a_stranger_key_and_a_short_signature_are_refused() {
        let flow_authority = Pubkey::new_unique();
        assert!(client(flow_authority)
            .decode(AttestOk {
                flow_authority: Pubkey::new_unique().to_string(),
                signature: base64::engine::general_purpose::STANDARD.encode([7u8; 64]),
                expiry_ts: 1_700_000_000,
            })
            .is_err());
        assert!(client(flow_authority)
            .decode(AttestOk {
                flow_authority: flow_authority.to_string(),
                signature: base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
                expiry_ts: 1_700_000_000,
            })
            .is_err());
    }
}
