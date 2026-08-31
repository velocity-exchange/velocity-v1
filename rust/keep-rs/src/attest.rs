//! Client for swift's `POST /attest` — the flow-authority co-signature
//! that marks a fill as attested retail flow.
//!
//! Quoters gating on `require_attested_flow` (the midpoint) only quote
//! attested transactions, and place_and_make_perp_order_v1 only grants
//! faster-than-default activation to them — so a swift-order fill that
//! carries the co-signature reaches strictly more liquidity. The flow:
//! build the fill with the flow authority as a read-only co-signer (hung
//! off a compute-budget instruction, whose accounts nothing parses), sign
//! our own slot against a fixed blockhash, post the transaction to swift,
//! and submit what comes back verbatim — both signatures are over the same
//! message, so nothing may be re-signed afterwards.
//!
//! Attestation failing is degradation, not failure: the caller falls back
//! to the plain unattested transaction and fills whatever quotes without
//! the marker.

use {
    base64::Engine,
    serde::Deserialize,
    solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction},
    std::time::Duration,
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
    transaction: String,
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

    pub fn flow_authority(&self) -> Pubkey {
        self.flow_authority
    }

    /// Ask swift to co-sign `tx` for `uuid`. Returns the co-signed
    /// transaction, retrying through the hold window (425 + retryAfterMs).
    pub async fn attest(
        &self,
        uuid: &str,
        tx: &VersionedTransaction,
    ) -> Result<VersionedTransaction, String> {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(bincode::serialize(tx).map_err(|e| e.to_string())?);
        let endpoint = format!("{}/attest", self.url);
        for attempt in 0..MAX_ATTEMPTS {
            let response = self
                .http
                .post(&endpoint)
                .json(&serde_json::json!({ "uuid": uuid, "transaction": encoded }))
                .send()
                .await
                .map_err(|e| format!("attest request failed: {e}"))?;
            let status = response.status();
            if status.is_success() {
                let ok: AttestOk = response
                    .json()
                    .await
                    .map_err(|e| format!("attest response undecodable: {e}"))?;
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(&ok.transaction)
                    .map_err(|e| format!("attested tx not base64: {e}"))?;
                return bincode::deserialize(&raw)
                    .map_err(|e| format!("attested tx undecodable: {e}"));
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
}
