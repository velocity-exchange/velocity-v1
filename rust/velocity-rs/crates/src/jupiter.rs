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
//! v2 also dropped `onlyDirectRoutes` and answers 200 for an unrecognized
//! parameter, so there is no way to ask it for a single-hop route. `maxAccounts`
//! is the remaining lever on how large a route may get, and it is always sent.
//!
//! The route is built for a named `taker`, so the returned quote is only
//! executable by the `user_authority` it was quoted for.
//!
//! Because a 200 is not by itself evidence that the route does what was asked,
//! every build is checked against its request before it is returned.
use std::{
    collections::BTreeMap,
    sync::{LazyLock, Once},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Deserializer};

use crate::{
    constants::ids::jupiter_mainnet_6,
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

/// Ceiling on the accounts a route may touch, so it still fits in a transaction
/// alongside the swap bracket (begin/end swap, ATA creation, the caller's own
/// instructions). Jupiter's own default (64) assumes the swap has the tx to
/// itself; this matches the TS SDK's `DEFAULT_SWAP_MAX_ACCOUNTS`.
const DEFAULT_MAX_ACCOUNTS: usize = 50;

/// Shared so the connection pool survives between quotes. Kept as the builder's
/// `Result` so a client that cannot be built fails the quote rather than
/// panicking a caller that only asked for a price.
static JUPITER_HTTP_CLIENT: LazyLock<reqwest::Result<reqwest::Client>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(JUPITER_REQUEST_TIMEOUT)
        .build()
});

/// Warned once, not once per quote
static MISSING_API_KEY_WARNING: Once = Once::new();

fn jupiter_http_client() -> SdkResult<&'static reqwest::Client> {
    JUPITER_HTTP_CLIENT.as_ref().map_err(|err| {
        log::error!("jupiter http client: {err:?}");
        SdkError::Generic(format!("jupiter http client: {err}"))
    })
}

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
///
/// A build's `otherInstructions` / `tipInstruction` are not represented: they
/// belong to Jupiter's own transaction landing, which this SDK never opts into,
/// and the swap bracket rejects any instruction it does not recognize. A build
/// carrying either is rejected rather than silently stripped.
#[derive(Clone, Debug)]
pub struct JupiterRouteInstructions {
    pub compute_budget_instructions: Vec<Instruction>,
    pub setup_instructions: Vec<Instruction>,
    /// Instruction performing the action of swapping
    pub swap_instruction: Instruction,
    pub cleanup_instruction: Option<Instruction>,
}

pub trait JupiterSwapApi {
    fn jupiter_swap_query(
        &self,
        user_authority: &Pubkey,
        amount: u64,
        slippage_bps: u16,
        in_market: u16,
        out_market: u16,
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
    /// * `excluded_dexes` - Optional comma-separated string of DEX names to exclude from routing
    /// * `max_accounts` - Cap on the number of accounts the route may touch,
    ///   defaulting to [`DEFAULT_MAX_ACCOUNTS`]
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing `JupiterSwapInfo` with the swap instructions and route details
    /// if successful, or a `SdkError` if the route does not answer the request or the operation
    /// fails.
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
        excluded_dexes: Option<String>,
        max_accounts: Option<usize>,
    ) -> SdkResult<JupiterSwapInfo> {
        let jupiter_url =
            std::env::var("JUPITER_API_URL").unwrap_or(DEFAULT_JUPITER_API_URL.into());

        let in_market = self.try_get_spot_market_account(in_market)?;
        let out_market = self.try_get_spot_market_account(out_market)?;

        let request = SwapRequest {
            input_mint: in_market.mint,
            output_mint: out_market.mint,
            amount,
            slippage_bps,
        };

        let mut query = vec![
            ("inputMint", request.input_mint.to_string()),
            ("outputMint", request.output_mint.to_string()),
            ("amount", amount.to_string()),
            ("slippageBps", slippage_bps.to_string()),
            ("taker", user_authority.to_string()),
            (
                "maxAccounts",
                max_accounts.unwrap_or(DEFAULT_MAX_ACCOUNTS).to_string(),
            ),
        ];
        if let Some(excluded_dexes) = excluded_dexes {
            query.push(("excludeDexes", excluded_dexes));
        }

        let mut http_request = jupiter_http_client()?
            .get(format!("{jupiter_url}/build"))
            .query(&query);
        match std::env::var("JUPITER_API_KEY") {
            Ok(api_key) => http_request = http_request.header("x-api-key", api_key),
            // once: this runs on every quote, and a keeper quotes in a loop
            Err(_) => MISSING_API_KEY_WARNING.call_once(|| {
                log::warn!(
                    "JUPITER_API_KEY not set. Jupiter API requests may fail after Jan 31, 2026. \
                     Get a free API key at https://portal.jup.ag"
                )
            }),
        }

        let response = http_request.send().await.map_err(|err| {
            log::error!("jupiter api request: {err:?}");
            SdkError::Generic(err.to_string())
        })?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|err| {
            log::error!("jupiter api response: {err:?}");
            SdkError::Generic(err.to_string())
        })?;

        parse_build(status, &body)?.into_swap_info(&request)
    }
}

/// Reads a `/swap/v2/build` response, or says why it is not one
///
/// v2 reports a failure both as a non-2xx and as a 200 carrying an error body,
/// so the body is checked before the status. A body that is neither — an HTML
/// gateway page, say — is reported with its content, since the status alone
/// does not identify where it came from.
fn parse_build(status: u16, body: &str) -> SdkResult<BuildResponse> {
    if let Some(message) = serde_json::from_str::<ErrorBody>(body)
        .ok()
        .and_then(|err| err.describe())
    {
        return Err(build_failed(status, message));
    }
    if !(200..300).contains(&status) {
        return Err(build_failed(status, truncate(body)));
    }

    // A v2 build carries the route's instructions, so a body missing them is
    // unusable rather than merely uninteresting — deserializing the whole
    // response is what rejects it, here instead of on-chain.
    serde_json::from_str(body).map_err(|err| {
        build_failed(
            status,
            format!("unreadable response: {err}: {}", truncate(body)),
        )
    })
}

/// A response that is not a usable build
fn build_failed(status: u16, detail: impl std::fmt::Display) -> SdkError {
    log::error!("jupiter build failed ({status}): {detail}");
    SdkError::Generic(format!("jupiter build failed ({status}): {detail}"))
}

/// The swap a build was asked for, which it is checked against on the way back
struct SwapRequest {
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount: u64,
    slippage_bps: u16,
}

/// A `/swap/v2/build` response
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildResponse {
    #[serde(flatten)]
    quote: JupiterQuote,
    /// Response-only in v2 — there is no `swapMode` request parameter, so this is
    /// the only place a non-ExactIn route announces itself.
    #[serde(default)]
    swap_mode: Option<String>,
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

impl BuildResponse {
    /// Checks that the build executes the swap it was asked for, then converts it
    ///
    /// A v2 build is not self-evidently the answer to its request: the endpoint
    /// ignores parameters it does not recognize, and the amount it reports is
    /// what `begin_swap` releases from the vault. So the route's own account of
    /// itself — mints, amount, slippage, swap mode, and the program that executes
    /// it — is checked against the request here, off-chain, rather than left to
    /// surface as an `InvalidSwap` (or a correctly-executed wrong swap) on-chain.
    fn into_swap_info(self, request: &SwapRequest) -> SdkResult<JupiterSwapInfo> {
        let quote = &self.quote;
        if quote.input_mint != request.input_mint || quote.output_mint != request.output_mint {
            return Err(mismatch(format!(
                "route swaps {}->{}, requested {}->{}",
                quote.input_mint, quote.output_mint, request.input_mint, request.output_mint
            )));
        }
        if quote.in_amount != request.amount {
            return Err(mismatch(format!(
                "route spends {}, requested {}",
                quote.in_amount, request.amount
            )));
        }
        if quote.slippage_bps != request.slippage_bps {
            return Err(mismatch(format!(
                "route is priced at {}bps slippage, requested {}bps",
                quote.slippage_bps, request.slippage_bps
            )));
        }
        // Absent is taken as ExactIn: it is the only mode the endpoint builds.
        if let Some(swap_mode) = self.swap_mode.as_deref().filter(|mode| *mode != "ExactIn") {
            return Err(mismatch(format!("route is {swap_mode}, requested ExactIn")));
        }
        if self.swap_instruction.program_id != jupiter_mainnet_6::ID {
            return Err(mismatch(format!(
                "route executes on {}, expected jupiter v6 ({})",
                self.swap_instruction.program_id,
                jupiter_mainnet_6::ID
            )));
        }
        // Neither can be forwarded — the swap bracket rejects any instruction it
        // does not recognize — and neither should exist, since the SDK opts into
        // no feature that produces one. Dropping them would build a transaction
        // missing a step the route needs.
        if !self.other_instructions.is_empty() {
            return Err(mismatch(format!(
                "route carries {} unsupported auxiliary instruction(s)",
                self.other_instructions.len()
            )));
        }
        if self.tip_instruction.is_some() {
            return Err(mismatch("route carries an unsupported tip instruction"));
        }

        let luts = self
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
            quote: self.quote,
            ixs: JupiterRouteInstructions {
                compute_budget_instructions: self
                    .compute_budget_instructions
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                setup_instructions: self
                    .setup_instructions
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                swap_instruction: self.swap_instruction.into(),
                cleanup_instruction: self.cleanup_instruction.map(Into::into),
            },
            luts,
        })
    }
}

/// A build that does not answer the request it was given
fn mismatch(detail: impl std::fmt::Display) -> SdkError {
    log::error!("jupiter build mismatch: {detail}");
    SdkError::Generic(format!("jupiter build mismatch: {detail}"))
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
                    // No issues to unpack: the object itself is the only detail
                    // there is, so it is rendered as json (never `[object
                    // Object]`) and bounded like any other unrecognized body.
                    true => format!(
                        "{name}: {}",
                        truncate(&serde_json::Value::Object(error.clone()).to_string())
                    ),
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

    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const SOL: &str = "So11111111111111111111111111111111111111112";

    /// The request [`BUILD_RESPONSE`] is the answer to
    fn request() -> SwapRequest {
        SwapRequest {
            input_mint: USDC.parse().unwrap(),
            output_mint: SOL.parse().unwrap(),
            amount: 10_000_000,
            slippage_bps: 10,
        }
    }

    /// Converts a body edited away from [`BUILD_RESPONSE`], expecting a rejection
    fn expect_rejected(body: &str) -> String {
        assert_ne!(body, BUILD_RESPONSE, "fixture edit applied");
        let build: BuildResponse = serde_json::from_str(body).expect("parses");
        build
            .into_swap_info(&request())
            .err()
            .expect("rejected")
            .to_string()
    }

    #[test]
    fn deserializes_a_v2_build() {
        let build: BuildResponse = serde_json::from_str(BUILD_RESPONSE).expect("parses");
        let info = build.into_swap_info(&request()).expect("converts");

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
        let info = build.into_swap_info(&request()).expect("converts");
        assert!(info.luts.is_empty());
    }

    /// v2 has no `onlyDirectRoutes` and ignores parameters it does not know, so a
    /// route the caller cannot constrain is the one that must be checked.
    #[test]
    fn rejects_a_build_for_another_pair() {
        let body = BUILD_RESPONSE.replace(
            r#""outputMint": "So11111111111111111111111111111111111111112","#,
            r#""outputMint": "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So","#,
        );
        assert!(expect_rejected(&body).contains("route swaps"));
    }

    /// `in_amount` is what `begin_swap` releases from the vault
    #[test]
    fn rejects_a_build_for_another_amount() {
        let body = BUILD_RESPONSE.replace(r#""inAmount": "10000000""#, r#""inAmount": "20000000""#);
        assert!(expect_rejected(&body).contains("route spends 20000000"));
    }

    #[test]
    fn rejects_a_build_at_another_slippage() {
        let body = BUILD_RESPONSE.replace(r#""slippageBps": 10"#, r#""slippageBps": 500"#);
        assert!(expect_rejected(&body).contains("500bps"));
    }

    /// The endpoint answers 200 to an ExactOut request, spending the amount as
    /// the input; the mode it reports is the only signal that it did so.
    #[test]
    fn rejects_an_exact_out_build() {
        let body = BUILD_RESPONSE.replace(r#""swapMode": "ExactIn""#, r#""swapMode": "ExactOut""#);
        assert!(expect_rejected(&body).contains("route is ExactOut"));
    }

    #[test]
    fn rejects_a_build_from_an_unexpected_program() {
        let body = BUILD_RESPONSE.replace(
            r#""programId": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
            "accounts": ["#,
            r#""programId": "JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB",
            "accounts": ["#,
        );
        assert!(expect_rejected(&body).contains("expected jupiter v6"));
    }

    /// An auxiliary instruction cannot go in the swap bracket, so a build needing
    /// one has no executable transaction — it must fail here, not silently ship
    /// a route missing a step.
    #[test]
    fn rejects_a_build_carrying_other_instructions() {
        let body = BUILD_RESPONSE.replace(
            r#""otherInstructions": [],"#,
            r#""otherInstructions": [
                {
                    "programId": "11111111111111111111111111111111",
                    "accounts": [],
                    "data": "AQ=="
                }
            ],"#,
        );
        assert!(expect_rejected(&body).contains("1 unsupported auxiliary instruction"));
    }

    #[test]
    fn rejects_a_build_carrying_a_tip_instruction() {
        let body = BUILD_RESPONSE.replace(
            r#""tipInstruction": null,"#,
            r#""tipInstruction": {
                "programId": "11111111111111111111111111111111",
                "accounts": [],
                "data": "AQ=="
            },"#,
        );
        assert!(expect_rejected(&body).contains("tip instruction"));
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

    /// An error object carrying no `issues` still has to read as json rather than
    /// as a serde debug dump or a stringified object
    #[test]
    fn describes_an_error_object_without_issues() {
        let body: ErrorBody =
            serde_json::from_str(r#"{"error":{"name":"ZodError","cause":"bad taker"}}"#)
                .expect("parses");
        let described = body.describe().expect("described");
        assert!(described.starts_with("ZodError: {"), "{described}");
        assert!(described.contains(r#""cause":"bad taker""#), "{described}");
        assert!(!described.contains("[object Object]"), "{described}");
    }

    #[test]
    fn reads_a_successful_build() {
        assert!(parse_build(200, BUILD_RESPONSE).is_ok());
    }

    /// v2 reports failures on a 200, so the body decides before the status does
    #[test]
    fn reads_an_error_body_carried_on_a_200() {
        let err = parse_build(200, r#"{"error":"Could not find any route"}"#)
            .err()
            .expect("rejected")
            .to_string();
        assert!(err.contains("(200)"), "{err}");
        assert!(err.contains("Could not find any route"), "{err}");
    }

    /// A gateway's HTML is neither an error body nor a build; the status alone
    /// does not say who answered, so the body has to survive into the error.
    #[test]
    fn reports_an_unreadable_200_with_its_body() {
        let err = parse_build(200, "<html><body>502 Bad Gateway</body></html>")
            .err()
            .expect("rejected")
            .to_string();
        assert!(err.contains("unreadable response"), "{err}");
        assert!(err.contains("502 Bad Gateway"), "{err}");
    }

    /// An unrecognized body is bounded so one bad response can't flood the log
    #[test]
    fn truncates_an_unrecognized_error_body() {
        let body = "x".repeat(2_000);
        let err = parse_build(500, &body).err().expect("rejected").to_string();
        assert!(err.contains('…'), "{err}");
        assert!(err.len() < 700, "{} chars", err.len());
    }
}
