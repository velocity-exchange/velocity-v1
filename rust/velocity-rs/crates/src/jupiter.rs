//! Jupiter Swap API v2 helpers
//!
//! `GET /swap/v2/build` answers a quote and the instructions that execute it in
//! one round trip, replacing v1's `GET /quote` -> `POST /swap-instructions` pair.
//! It also returns each lookup table's addresses inline, so building a swap tx
//! needs no account fetch at all.
//!
//! The endpoint is **ExactIn-only** — it dropped `swapMode` from its contract and,
//! sent `ExactOut`, answers 200 having spent the requested amount as the *input*.
//! There is therefore no swap-mode parameter here: an amount is always an input
//! amount. Use the `titan` module (feature `titan`) for ExactOut.
//!
//! The route is built for a named `taker`, so the returned quote is only
//! executable by the `user_authority` it was quoted for.
use std::{collections::BTreeMap, sync::LazyLock, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Deserializer};

use crate::{
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        message::AddressLookupTableAccount,
        pubkey::Pubkey,
    },
    types::{SdkError, SdkResult},
    VelocityClient,
};

/// Default Jupiter API url (lite-api.jup.ag is deprecated as of Jan 31, 2026)
/// See: https://dev.jup.ag/portal/migrate-from-lite-api
const DEFAULT_JUPITER_API_URL: &str = "https://api.jup.ag/swap/v2";

/// A quote goes stale in seconds, so a hung request is worth less than a retry
const JUPITER_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared so the connection pool survives between quotes
static JUPITER_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(JUPITER_REQUEST_TIMEOUT)
        .build()
        .expect("jupiter http client")
});

/// jupiter swap IXs and metadata for building a swap Tx
pub struct JupiterSwapInfo {
    pub quote: JupiterQuote,
    pub ixs: JupiterRouteInstructions,
    pub luts: Vec<AddressLookupTableAccount>,
}

/// The quote half of a `/swap/v2/build` response
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterQuote {
    #[serde(deserialize_with = "deser_pubkey")]
    pub input_mint: Pubkey,
    #[serde(deserialize_with = "deser_pubkey")]
    pub output_mint: Pubkey,
    /// Amount spent. Always the requested amount — the endpoint is ExactIn-only.
    #[serde(deserialize_with = "deser_u64")]
    pub in_amount: u64,
    #[serde(deserialize_with = "deser_u64")]
    pub out_amount: u64,
    /// `out_amount` less the slippage tolerance: the route's minimum received
    #[serde(deserialize_with = "deser_u64")]
    pub other_amount_threshold: u64,
    pub slippage_bps: u16,
    #[serde(default)]
    pub price_impact_pct: Option<String>,
}

/// The instructions a `/swap/v2/build` route is made of, in execution order
#[derive(Clone, Debug)]
pub struct JupiterRouteInstructions {
    pub compute_budget_instructions: Vec<Instruction>,
    pub setup_instructions: Vec<Instruction>,
    /// Instruction performing the action of swapping
    pub swap_instruction: Instruction,
    pub cleanup_instruction: Option<Instruction>,
    /// Instructions that are not part of the route itself — currently only a Jito tip
    pub other_instructions: Vec<Instruction>,
    /// Set only when opted into Jupiter's own transaction landing, which this SDK does not
    pub tip_instruction: Option<Instruction>,
}

pub trait JupiterSwapApi {
    fn jupiter_swap_query(
        &self,
        user_authority: &Pubkey,
        amount: u64,
        slippage_bps: u16,
        in_market: u16,
        out_market: u16,
        only_direct_routes: Option<bool>,
        excluded_dexes: Option<String>,
        max_accounts: Option<usize>,
    ) -> impl std::future::Future<Output = SdkResult<JupiterSwapInfo>> + Send;
}

impl JupiterSwapApi for VelocityClient {
    /// Fetch Jupiter swap ixs and metadata for a token swap
    ///
    /// Queries `GET /swap/v2/build` for the optimal route between two tokens and
    /// the instructions that execute it.
    ///
    /// # Arguments
    ///
    /// * `user_authority` - The public key of the user's wallet that will execute the swap.
    ///   The route is built for this wallet and is not executable by another.
    /// * `amount` - The amount of input tokens to swap, in native units (smallest denomination)
    /// * `slippage_bps` - Maximum allowed slippage in basis points (1 bp = 0.01%)
    /// * `in_market` - The market index of the token to swap from
    /// * `out_market` - The market index of the token to swap to
    /// * `only_direct_routes` - If Some(true), only consider direct swap routes between the tokens
    /// * `excluded_dexes` - Optional comma-separated string of DEX names to exclude from routing
    /// * `max_accounts` - Optional cap on the number of accounts the route may touch
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing `JupiterSwapInfo` with the swap instructions and route details
    /// if successful, or a `SdkError` if the operation fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use velocity_rs::{Context, VelocityClient, RpcClient, Wallet};
    /// use velocity_rs::jupiter::JupiterSwapApi;
    /// use velocity_rs::types::SdkResult;
    /// use solana_keypair::Keypair;
    ///
    /// # async fn run() -> SdkResult<()> {
    /// let wallet = Wallet::new(Keypair::new());
    /// let client = VelocityClient::new(
    ///     Context::MainNet,
    ///     RpcClient::new("https://api.mainnet-beta.solana.com".into()),
    ///     wallet.clone(),
    /// )
    /// .await?;
    ///
    /// let swap_info = client
    ///     .jupiter_swap_query(
    ///         wallet.authority(),
    ///         1_000_000, // 1 USDC in
    ///         50,  // 0.5% slippage
    ///         0,   // in spot market index (e.g. USDC)
    ///         1,   // out spot market index (e.g. SOL)
    ///         Some(true),
    ///         None,
    ///         None,
    ///     )
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    async fn jupiter_swap_query(
        &self,
        user_authority: &Pubkey,
        amount: u64,
        slippage_bps: u16,
        in_market: u16,
        out_market: u16,
        only_direct_routes: Option<bool>,
        excluded_dexes: Option<String>,
        max_accounts: Option<usize>,
    ) -> SdkResult<JupiterSwapInfo> {
        let jupiter_url =
            std::env::var("JUPITER_API_URL").unwrap_or(DEFAULT_JUPITER_API_URL.into());

        let in_market = self.try_get_spot_market_account(in_market)?;
        let out_market = self.try_get_spot_market_account(out_market)?;

        let mut query = vec![
            ("inputMint", in_market.mint.to_string()),
            ("outputMint", out_market.mint.to_string()),
            ("amount", amount.to_string()),
            ("slippageBps", slippage_bps.to_string()),
            ("taker", user_authority.to_string()),
        ];
        if let Some(only_direct_routes) = only_direct_routes {
            query.push(("onlyDirectRoutes", only_direct_routes.to_string()));
        }
        if let Some(max_accounts) = max_accounts {
            query.push(("maxAccounts", max_accounts.to_string()));
        }
        if let Some(excluded_dexes) = excluded_dexes {
            query.push(("excludeDexes", excluded_dexes));
        }

        let mut request = JUPITER_HTTP_CLIENT
            .get(format!("{jupiter_url}/build"))
            .query(&query);
        match std::env::var("JUPITER_API_KEY") {
            Ok(api_key) => request = request.header("x-api-key", api_key),
            Err(_) => log::warn!(
                "JUPITER_API_KEY not set. Jupiter API requests may fail after Jan 31, 2026. \
                 Get a free API key at https://portal.jup.ag"
            ),
        }

        let response = request.send().await.map_err(|err| {
            log::error!("jupiter api request: {err:?}");
            SdkError::Generic(err.to_string())
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|err| {
            log::error!("jupiter api response: {err:?}");
            SdkError::Generic(err.to_string())
        })?;

        // v2 reports a failure both as a non-2xx and as a 200 carrying an error
        // body, so the body is checked before the status.
        if let Some(message) = serde_json::from_str::<ErrorBody>(&body)
            .ok()
            .and_then(|err| err.describe())
        {
            log::error!("jupiter build failed ({status}): {message}");
            return Err(SdkError::Generic(format!(
                "jupiter build failed ({status}): {message}"
            )));
        }
        if !status.is_success() {
            log::error!("jupiter build failed ({status}): {body}");
            return Err(SdkError::Generic(format!(
                "jupiter build failed ({status}): {}",
                truncate(&body)
            )));
        }

        // A v2 build carries the route's instructions, so a body missing them is
        // unusable rather than merely uninteresting — deserializing the whole
        // response is what rejects it, here instead of on-chain.
        let build: BuildResponse = serde_json::from_str(&body).map_err(|err| {
            log::error!("jupiter build response: {err:?}");
            SdkError::Generic(format!("jupiter build response: {err}"))
        })?;

        build.try_into()
    }
}

/// A `/swap/v2/build` response
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildResponse {
    #[serde(flatten)]
    quote: JupiterQuote,
    compute_budget_instructions: Vec<ApiInstruction>,
    setup_instructions: Vec<ApiInstruction>,
    swap_instruction: ApiInstruction,
    cleanup_instruction: Option<ApiInstruction>,
    #[serde(default)]
    other_instructions: Vec<ApiInstruction>,
    #[serde(default)]
    tip_instruction: Option<ApiInstruction>,
    /// v2 returns each lookup table's addresses inline, which is everything an
    /// `AddressLookupTableAccount` holds, so the tables need no fetch. The
    /// trade-off is that a table deactivated between the build and the send is
    /// no longer noticed here — it surfaces when the tx is simulated instead.
    ///
    /// Null or absent when the route needs no lookup tables. Ordered, so a given
    /// route always yields the same LUT list.
    #[serde(default)]
    addresses_by_lookup_table_address: Option<BTreeMap<String, Vec<String>>>,
}

impl TryFrom<BuildResponse> for JupiterSwapInfo {
    type Error = SdkError;

    fn try_from(build: BuildResponse) -> SdkResult<Self> {
        let luts = build
            .addresses_by_lookup_table_address
            .unwrap_or_default()
            .iter()
            .map(|(key, addresses)| {
                Ok(AddressLookupTableAccount {
                    key: parse_pubkey(key)?,
                    addresses: addresses
                        .iter()
                        .map(|address| parse_pubkey(address))
                        .collect::<SdkResult<Vec<Pubkey>>>()?,
                })
            })
            .collect::<SdkResult<Vec<AddressLookupTableAccount>>>()?;

        Ok(JupiterSwapInfo {
            quote: build.quote,
            ixs: JupiterRouteInstructions {
                compute_budget_instructions: build
                    .compute_budget_instructions
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                setup_instructions: build
                    .setup_instructions
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                swap_instruction: build.swap_instruction.into(),
                cleanup_instruction: build.cleanup_instruction.map(Into::into),
                other_instructions: build
                    .other_instructions
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                tip_instruction: build.tip_instruction.map(Into::into),
            },
            luts,
        })
    }
}

/// An instruction as the Jupiter API encodes it on the wire
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiInstruction {
    #[serde(deserialize_with = "deser_pubkey")]
    program_id: Pubkey,
    accounts: Vec<ApiAccountMeta>,
    #[serde(deserialize_with = "deser_base64")]
    data: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiAccountMeta {
    #[serde(deserialize_with = "deser_pubkey")]
    pubkey: Pubkey,
    is_signer: bool,
    is_writable: bool,
}

impl From<ApiInstruction> for Instruction {
    fn from(value: ApiInstruction) -> Self {
        Instruction {
            program_id: value.program_id,
            accounts: value.accounts.into_iter().map(Into::into).collect(),
            data: value.data,
        }
    }
}

impl From<ApiAccountMeta> for AccountMeta {
    fn from(value: ApiAccountMeta) -> Self {
        AccountMeta {
            pubkey: value.pubkey,
            is_signer: value.is_signer,
            is_writable: value.is_writable,
        }
    }
}

/// The error shapes a v2 response can carry
///
/// Three unrelated ones: a Zod validation object (`{ error: { issues, name } }`),
/// a rate-limit body (`{ code, message }`), and v1's `{ error, errorCode }`.
/// Every field is optional so a successful body parses into this too, and
/// [`Self::describe`] answers `None` for it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    error: Option<serde_json::Value>,
    error_code: Option<String>,
    message: Option<String>,
}

impl ErrorBody {
    /// Renders whatever the body says went wrong, or `None` if it says nothing
    fn describe(&self) -> Option<String> {
        match &self.error {
            // A validation failure. Rendering the object itself would read as a
            // serde_json dump, so the issues are unpacked into `path: message`.
            Some(serde_json::Value::Object(error)) => {
                let issues: Vec<String> = error
                    .get("issues")
                    .and_then(serde_json::Value::as_array)
                    .map(|issues| {
                        issues
                            .iter()
                            .filter_map(|issue| {
                                let message = issue.get("message")?.as_str()?;
                                let path = issue
                                    .get("path")
                                    .and_then(serde_json::Value::as_array)
                                    .map(|path| {
                                        path.iter()
                                            // a bare `to_string()` would quote the strings
                                            .map(|part| match part {
                                                serde_json::Value::String(part) => part.clone(),
                                                part => part.to_string(),
                                            })
                                            .collect::<Vec<String>>()
                                            .join(".")
                                    })
                                    .unwrap_or_default();
                                Some(match path.is_empty() {
                                    true => message.to_string(),
                                    false => format!("{path}: {message}"),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let name = error
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("error");
                Some(match issues.is_empty() {
                    true => format!("{name}: {}", serde_json::Value::Object(error.clone())),
                    false => format!("{name}: {}", issues.join("; ")),
                })
            }
            // An empty-string `error` is as useless as a missing one, so it falls
            // through to the next candidate rather than rendering as nothing.
            Some(serde_json::Value::String(error)) if !error.is_empty() => Some(error.clone()),
            _ => self
                .error_code
                .clone()
                .or_else(|| self.message.clone())
                .filter(|message| !message.is_empty()),
        }
    }
}

/// Bounds an unrecognized error body so it stays readable in a log line
fn truncate(body: &str) -> String {
    const MAX: usize = 512;
    match body.char_indices().nth(MAX) {
        Some((idx, _)) => format!("{}…", &body[..idx]),
        None => body.to_string(),
    }
}

fn parse_pubkey(value: &str) -> SdkResult<Pubkey> {
    value
        .parse()
        .map_err(|_| SdkError::Generic(format!("jupiter build: invalid pubkey: {value}")))
}

fn deser_pubkey<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Pubkey, D::Error> {
    let value = String::deserialize(deserializer)?;
    value.parse().map_err(serde::de::Error::custom)
}

fn deser_base64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let value = String::deserialize(deserializer)?;
    STANDARD.decode(value).map_err(serde::de::Error::custom)
}

/// v2 reports token amounts as JSON strings; a bare number is accepted too.
fn deser_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(u64),
    }

    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(value) => value.parse().map_err(serde::de::Error::custom),
        StringOrNumber::Number(value) => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/swap/v2/build` body, trimmed to the fields the SDK reads
    const BUILD_RESPONSE: &str = r#"{
        "inputMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        "outputMint": "So11111111111111111111111111111111111111112",
        "inAmount": "10000000",
        "outAmount": "72510138",
        "otherAmountThreshold": "72437627",
        "swapMode": "ExactIn",
        "slippageBps": 10,
        "priceImpactPct": "0.0001",
        "routePlan": [{ "percent": 100, "bps": 10000 }],
        "computeBudgetInstructions": [
            {
                "programId": "ComputeBudget111111111111111111111111111111",
                "accounts": [],
                "data": "AwAAAAAAAAA="
            }
        ],
        "setupInstructions": [
            {
                "programId": "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
                "accounts": [
                    {
                        "pubkey": "9JtczxrJjPM4J1xooxr2rFXmRivarb4BwjNiBgXDwe2p",
                        "isSigner": true,
                        "isWritable": true
                    }
                ],
                "data": "AQ=="
            }
        ],
        "swapInstruction": {
            "programId": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
            "accounts": [
                {
                    "pubkey": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                    "isSigner": false,
                    "isWritable": false
                },
                {
                    "pubkey": "9JtczxrJjPM4J1xooxr2rFXmRivarb4BwjNiBgXDwe2p",
                    "isSigner": true,
                    "isWritable": true
                }
            ],
            "data": "wSCbM0HWnIE="
        },
        "cleanupInstruction": {
            "programId": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "accounts": [],
            "data": "CQ=="
        },
        "otherInstructions": [],
        "tipInstruction": null,
        "addressesByLookupTableAddress": {
            "So11111111111111111111111111111111111111112": [
                "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            ],
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": [
                "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
                "ComputeBudget111111111111111111111111111111"
            ]
        }
    }"#;

    #[test]
    fn deserializes_a_v2_build() {
        let build: BuildResponse = serde_json::from_str(BUILD_RESPONSE).expect("parses");
        let info: JupiterSwapInfo = build.try_into().expect("converts");

        // string-encoded amounts
        assert_eq!(info.quote.in_amount, 10_000_000);
        assert_eq!(info.quote.out_amount, 72_510_138);
        assert_eq!(info.quote.other_amount_threshold, 72_437_627);
        assert_eq!(info.quote.slippage_bps, 10);
        assert_eq!(
            info.quote.input_mint,
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
                .parse()
                .unwrap()
        );

        let swap_ix = &info.ixs.swap_instruction;
        assert_eq!(
            swap_ix.program_id,
            "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"
                .parse()
                .unwrap()
        );
        assert_eq!(swap_ix.data, STANDARD.decode("wSCbM0HWnIE=").unwrap());
        assert_eq!(swap_ix.accounts.len(), 2);
        assert!(!swap_ix.accounts[0].is_signer);
        assert!(swap_ix.accounts[1].is_signer && swap_ix.accounts[1].is_writable);

        assert_eq!(info.ixs.compute_budget_instructions.len(), 1);
        assert_eq!(info.ixs.setup_instructions.len(), 1);
        assert!(info.ixs.cleanup_instruction.is_some());
        assert!(info.ixs.other_instructions.is_empty());
        assert!(info.ixs.tip_instruction.is_none());

        // LUT addresses come inline — no account fetch, and a stable order
        assert_eq!(info.luts.len(), 2);
        assert_eq!(
            info.luts[0].key,
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
                .parse()
                .unwrap()
        );
        assert_eq!(info.luts[0].addresses.len(), 2);
        assert_eq!(info.luts[1].addresses.len(), 1);
    }

    /// A route needing no lookup tables reports the field as null, which must not
    /// read as a malformed build.
    #[test]
    fn accepts_a_build_with_no_lookup_tables() {
        let body = BUILD_RESPONSE.replace(
            r#""addressesByLookupTableAddress": {
            "So11111111111111111111111111111111111111112": [
                "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            ],
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": [
                "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
                "ComputeBudget111111111111111111111111111111"
            ]
        }"#,
            r#""addressesByLookupTableAddress": null"#,
        );
        assert!(
            !body.contains(r#""addressesByLookupTableAddress": {"#),
            "fixture replacement applied"
        );

        let build: BuildResponse = serde_json::from_str(&body).expect("parses");
        let info: JupiterSwapInfo = build.try_into().expect("converts");
        assert!(info.luts.is_empty());
    }

    #[test]
    fn a_successful_body_describes_no_error() {
        let body: ErrorBody = serde_json::from_str(BUILD_RESPONSE).expect("parses");
        assert_eq!(body.describe(), None);
    }

    #[test]
    fn describes_a_validation_error() {
        let body: ErrorBody = serde_json::from_str(
            r#"{"error":{"name":"ZodError","issues":[{"path":["taker"],"message":"Required"}]}}"#,
        )
        .expect("parses");
        assert_eq!(
            body.describe().as_deref(),
            Some("ZodError: taker: Required")
        );
    }

    #[test]
    fn describes_a_rate_limit_error() {
        let body: ErrorBody =
            serde_json::from_str(r#"{"code":429,"message":"Too many requests"}"#).expect("parses");
        assert_eq!(body.describe().as_deref(), Some("Too many requests"));
    }

    #[test]
    fn describes_a_v1_shaped_error() {
        let body: ErrorBody =
            serde_json::from_str(r#"{"error":"Could not find any route","errorCode":"NO_ROUTE"}"#)
                .expect("parses");
        assert_eq!(body.describe().as_deref(), Some("Could not find any route"));
    }
}
