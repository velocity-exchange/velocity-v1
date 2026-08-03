//! Jupiter SDK helpers
//!
//! Talks to the Jupiter Swap API **v2** Router endpoint `GET /swap/v2/build`, which returns the
//! quote and the raw swap instructions in a single round trip (v1 needed `GET /quote` followed by
//! `POST /swap-instructions`). The response is mapped back onto the `jupiter-swap-api-client`
//! types so [`JupiterSwapInfo`] keeps the same public shape as under v1.
use crate::{
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        message::AddressLookupTableAccount,
        pubkey::Pubkey,
    },
    types::{SdkError, SdkResult},
    utils, VelocityClient,
};
use base64::Engine;
pub use jupiter_swap_api_client::{
    quote::{QuoteResponse, SwapMode},
    swap::SwapInstructionsResponse,
    transaction_config::TransactionConfig,
    JupiterSwapApiClient,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::{collections::HashMap, str::FromStr};

/// Default Jupiter API url — the v2 Router base. `/build` is appended by the query below.
/// See: https://dev.jup.ag/docs/swap-api
const DEFAULT_JUPITER_API_URL: &str = "https://api.jup.ag/swap/v2";

/// jupiter swap IXs and metadata for building a swap Tx
pub struct JupiterSwapInfo {
    pub quote: QuoteResponse,
    pub ixs: SwapInstructionsResponse,
    pub luts: Vec<AddressLookupTableAccount>,
}

pub trait JupiterSwapApi {
    fn jupiter_swap_query(
        &self,
        user_authority: &Pubkey,
        amount: u64,
        swap_mode: SwapMode,
        in_market: u16,
        out_market: u16,
        slippage_bps: u16,
        only_direct_routes: Option<bool>,
        excluded_dexes: Option<String>,
        transaction_config: Option<TransactionConfig>,
    ) -> impl std::future::Future<Output = SdkResult<JupiterSwapInfo>> + Send;
}

impl JupiterSwapApi for VelocityClient {
    /// Fetch Jupiter swap ixs and metadata for a token swap
    ///
    /// Issues a single `GET {JUPITER_API_URL}/build` (Jupiter Swap API v2) to get the optimal swap
    /// route and its raw instructions, then hydrates the route's address lookup tables over RPC.
    ///
    /// # Arguments
    ///
    /// * `user_authority` - The public key of the user's wallet that will execute the swap
    ///   (sent as the v2 `taker` query param, which is required)
    /// * `amount` - The amount of input tokens to swap, in native units (smallest denomination)
    /// * `swap_mode` - The type of swap to perform (e.g. ExactIn, ExactOut)
    /// * `slippage_bps` - Maximum allowed slippage in basis points (1 bp = 0.01%)
    /// * `in_market` - The market index of the token to swap from
    /// * `out_market` - The market index of the token to swap to
    /// * `only_direct_routes` - If Some(true), only consider direct swap routes between the tokens
    /// * `excluded_dexes` - Optional comma-separated string of DEX names to exclude from routing
    /// * `transaction_config` - **Ignored.** v2 `/build` has no equivalent request body; the
    ///   parameter is retained for source compatibility with the v1 signature.
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
    /// use velocity_rs::jupiter::{JupiterSwapApi, SwapMode};
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
    ///         1_000_000, // 1 USDC
    ///         SwapMode::ExactIn,
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
        swap_mode: SwapMode,
        slippage_bps: u16,
        in_market: u16,
        out_market: u16,
        only_direct_routes: Option<bool>,
        excluded_dexes: Option<String>,
        _transaction_config: Option<TransactionConfig>,
    ) -> SdkResult<JupiterSwapInfo> {
        let jupiter_url =
            std::env::var("JUPITER_API_URL").unwrap_or(DEFAULT_JUPITER_API_URL.into());
        let api_key = std::env::var("JUPITER_API_KEY").ok();
        if api_key.is_none() {
            log::info!(
                "JUPITER_API_KEY not set; using the keyless Jupiter tier which rate-limits \
                 aggressively. Get a free API key at https://portal.jup.ag"
            );
        }

        let in_market = self.try_get_spot_market_account(in_market)?;
        let out_market = self.try_get_spot_market_account(out_market)?;

        // GET /swap/v2/build — quote + raw instructions in one call
        let mut query: Vec<(&str, String)> = vec![
            ("inputMint", in_market.mint.to_string()),
            ("outputMint", out_market.mint.to_string()),
            ("amount", amount.to_string()),
            ("slippageBps", slippage_bps.to_string()),
            (
                "swapMode",
                match swap_mode {
                    SwapMode::ExactIn => "ExactIn".to_string(),
                    SwapMode::ExactOut => "ExactOut".to_string(),
                },
            ),
            ("taker", user_authority.to_string()),
        ];
        if let Some(only_direct_routes) = only_direct_routes {
            query.push(("onlyDirectRoutes", only_direct_routes.to_string()));
        }
        if let Some(excluded_dexes) = excluded_dexes {
            query.push(("excludeDexes", excluded_dexes));
        }

        let mut request = reqwest::Client::new()
            .get(format!("{jupiter_url}/build"))
            .query(&query);
        if let Some(api_key) = api_key {
            request = request.header("x-api-key", api_key);
        }

        let response = request.send().await.map_err(|err| {
            log::error!("jupiter api request: {err:?}");
            SdkError::Generic(format!("jupiter /build request failed: {err}"))
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|err| {
            log::error!("jupiter api request: {err:?}");
            SdkError::Generic(format!("jupiter /build response body ({status}): {err}"))
        })?;

        if !status.is_success() {
            let msg = describe_jupiter_error(&body);
            log::error!("jupiter api request failed ({status}): {msg}");
            return Err(SdkError::Generic(format!("jupiter /build {status}: {msg}")));
        }

        let build: BuildResponse = serde_json::from_str(&body).map_err(|err| {
            // A 200 can still carry a routing-failure body, so surface that before the serde error.
            let msg = describe_jupiter_error(&body);
            log::error!("jupiter api response: {err:?}");
            SdkError::Generic(format!("jupiter /build response: {msg} ({err})"))
        })?;

        let (quote_response, swap_instructions) = build.try_into_sdk_types()?;

        let res = self
            .rpc()
            .get_multiple_accounts(swap_instructions.address_lookup_table_addresses.as_slice())
            .await?;

        let luts = res
            .iter()
            .zip(swap_instructions.address_lookup_table_addresses.iter())
            .map(|(acc, key)| {
                utils::deserialize_alt(*key, acc.as_ref().expect("deser LUT")).expect("deser LUT")
            })
            .collect();

        Ok(JupiterSwapInfo {
            luts,
            quote: quote_response,
            ixs: swap_instructions,
        })
    }
}

/// Turn any of the three Jupiter v2 error body shapes into a readable message.
///
/// * validation failure (400): `{"success":false,"error":{"issues":[{path,message}],"name":"ZodError"}}`
/// * rate limit (429): `{"code":…,"message":"…"}`
/// * routing failure: `{"error":"…","errorCode":"…"}` (the v1 shape)
///
/// Falls back to the raw body so nothing is ever silently swallowed.
fn describe_jupiter_error(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return truncate(body);
    };

    // ZodError: `error` is an object carrying `issues`
    if let Some(issues) = value
        .get("error")
        .and_then(|e| e.get("issues"))
        .and_then(|i| i.as_array())
    {
        let issues: Vec<String> = issues
            .iter()
            .map(|issue| {
                let path = issue
                    .get("path")
                    .and_then(|p| p.as_array())
                    .map(|p| {
                        p.iter()
                            .map(|seg| {
                                seg.as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| seg.to_string())
                            })
                            .collect::<Vec<_>>()
                            .join(".")
                    })
                    .unwrap_or_default();
                let message = issue
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("invalid");
                if path.is_empty() {
                    message.to_string()
                } else {
                    format!("{path}: {message}")
                }
            })
            .collect();
        return format!("validation error ({})", issues.join("; "));
    }

    // v1-style routing failure: `error` is a string, optionally with `errorCode`
    if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
        return match value.get("errorCode").and_then(|c| c.as_str()) {
            Some(code) => format!("{error} ({code})"),
            None => error.to_string(),
        };
    }

    // rate limit / generic gateway shape: `{code, message}`
    if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
        return match value.get("code") {
            Some(code) => format!("{message} (code {code})"),
            None => message.to_string(),
        };
    }

    truncate(body)
}

fn truncate(body: &str) -> String {
    const MAX: usize = 512;
    match body.char_indices().nth(MAX) {
        Some((idx, _)) => format!("{}…", &body[..idx]),
        None => body.to_string(),
    }
}

// --- `GET /swap/v2/build` response ---

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct BuildResponse {
    input_mint: String,
    output_mint: String,
    in_amount: String,
    out_amount: String,
    other_amount_threshold: String,
    swap_mode: SwapMode,
    slippage_bps: u16,
    /// Sent as a decimal string; `Decimal`'s deserializer also accepts a JSON number.
    price_impact_pct: Decimal,
    route_plan: Vec<BuildRoutePlanStep>,
    compute_budget_instructions: Vec<BuildInstruction>,
    setup_instructions: Vec<BuildInstruction>,
    swap_instruction: BuildInstruction,
    cleanup_instruction: Option<BuildInstruction>,
    #[serde(default)]
    other_instructions: Vec<BuildInstruction>,
    #[serde(default)]
    tip_instruction: Option<BuildInstruction>,
    #[serde(default)]
    addresses_by_lookup_table_address: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct BuildRoutePlanStep {
    swap_info: BuildSwapInfo,
    /// Split share of the hop. v2 sends a fractional percentage (e.g. `29.23`) alongside `bps`.
    percent: f64,
    #[serde(default)]
    bps: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct BuildSwapInfo {
    amm_key: String,
    label: String,
    input_mint: String,
    output_mint: String,
    in_amount: String,
    out_amount: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct BuildInstruction {
    program_id: String,
    accounts: Vec<BuildAccountMeta>,
    /// base64 encoded
    data: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct BuildAccountMeta {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}

fn parse_pubkey(field: &str, value: &str) -> SdkResult<Pubkey> {
    Pubkey::from_str(value)
        .map_err(|err| SdkError::Generic(format!("jupiter /build {field} '{value}': {err}")))
}

fn parse_u64(field: &str, value: &str) -> SdkResult<u64> {
    value
        .parse()
        .map_err(|err| SdkError::Generic(format!("jupiter /build {field} '{value}': {err}")))
}

impl BuildInstruction {
    fn try_into_instruction(self) -> SdkResult<Instruction> {
        let accounts = self
            .accounts
            .into_iter()
            .map(|acc| {
                Ok(AccountMeta {
                    pubkey: parse_pubkey("accounts[].pubkey", &acc.pubkey)?,
                    is_signer: acc.is_signer,
                    is_writable: acc.is_writable,
                })
            })
            .collect::<SdkResult<Vec<_>>>()?;
        Ok(Instruction {
            program_id: parse_pubkey("programId", &self.program_id)?,
            accounts,
            data: base64::engine::general_purpose::STANDARD.decode(self.data)?,
        })
    }
}

impl BuildResponse {
    /// Map the v2 build response onto the `jupiter-swap-api-client` types the crate's public API
    /// exposes, so [`JupiterSwapInfo`] is shape-identical to the v1 two-call flow.
    fn try_into_sdk_types(self) -> SdkResult<(QuoteResponse, SwapInstructionsResponse)> {
        let route_plan = self
            .route_plan
            .into_iter()
            .map(|step| {
                Ok(
                    jupiter_swap_api_client::route_plan_with_metadata::RoutePlanStep {
                        swap_info: jupiter_swap_api_client::route_plan_with_metadata::SwapInfo {
                            amm_key: parse_pubkey("routePlan[].ammKey", &step.swap_info.amm_key)?,
                            label: step.swap_info.label,
                            input_mint: parse_pubkey(
                                "routePlan[].inputMint",
                                &step.swap_info.input_mint,
                            )?,
                            output_mint: parse_pubkey(
                                "routePlan[].outputMint",
                                &step.swap_info.output_mint,
                            )?,
                            in_amount: parse_u64(
                                "routePlan[].inAmount",
                                &step.swap_info.in_amount,
                            )?,
                            out_amount: parse_u64(
                                "routePlan[].outAmount",
                                &step.swap_info.out_amount,
                            )?,
                            // v2 no longer reports per-hop fees
                            fee_amount: None,
                            fee_mint: None,
                        },
                        // `RoutePlanStep::percent` is a u8; v2 splits are fractional, so round the
                        // bps share (informational field — no consumer sizes a swap off it).
                        percent: step
                            .bps
                            .map(|bps| (bps as f64) / 100.0)
                            .unwrap_or(step.percent)
                            .round()
                            .clamp(0.0, 100.0) as u8,
                    },
                )
            })
            .collect::<SdkResult<Vec<_>>>()?;

        let quote = QuoteResponse {
            input_mint: parse_pubkey("inputMint", &self.input_mint)?,
            in_amount: parse_u64("inAmount", &self.in_amount)?,
            output_mint: parse_pubkey("outputMint", &self.output_mint)?,
            out_amount: parse_u64("outAmount", &self.out_amount)?,
            other_amount_threshold: parse_u64(
                "otherAmountThreshold",
                &self.other_amount_threshold,
            )?,
            swap_mode: self.swap_mode,
            slippage_bps: self.slippage_bps,
            // v1-only auto-slippage reporting; v2 has no equivalent
            computed_auto_slippage: None,
            uses_quote_minimizing_slippage: None,
            platform_fee: None,
            price_impact_pct: self.price_impact_pct,
            route_plan,
            // not reported by v2
            context_slot: 0,
            time_taken: 0.0,
        };

        // `tipInstruction` is a System-program transfer to a Jito tip account. It must never be
        // spliced into the swap bracket (the program rejects the tx with `InvalidSwap`), so fold it
        // into `other_instructions` where `build_jupiter_swap_ixs` already refuses to proceed.
        let other_instructions = self
            .other_instructions
            .into_iter()
            .chain(self.tip_instruction)
            .map(BuildInstruction::try_into_instruction)
            .collect::<SdkResult<Vec<_>>>()?;

        let address_lookup_table_addresses = self
            .addresses_by_lookup_table_address
            .keys()
            .map(|key| parse_pubkey("addressesByLookupTableAddress", key))
            .collect::<SdkResult<Vec<_>>>()?;

        let ixs = SwapInstructionsResponse {
            // v2 has no token-ledger flow
            token_ledger_instruction: None,
            compute_budget_instructions: self
                .compute_budget_instructions
                .into_iter()
                .map(BuildInstruction::try_into_instruction)
                .collect::<SdkResult<Vec<_>>>()?,
            setup_instructions: self
                .setup_instructions
                .into_iter()
                .map(BuildInstruction::try_into_instruction)
                .collect::<SdkResult<Vec<_>>>()?,
            swap_instruction: self.swap_instruction.try_into_instruction()?,
            cleanup_instruction: self
                .cleanup_instruction
                .map(BuildInstruction::try_into_instruction)
                .transpose()?,
            other_instructions,
            address_lookup_table_addresses,
            // v2 leaves fee/CU budgeting to the caller and reports no simulation
            prioritization_fee_lamports: 0,
            compute_unit_limit: 0,
            prioritization_type: None,
            dynamic_slippage_report: None,
            simulation_error: None,
        };

        Ok((quote, ixs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JUP_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
    const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
    const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";

    /// Trimmed `GET https://api.jup.ag/swap/v2/build` response (USDC -> USDT, ExactIn, 50bps),
    /// account lists shortened.
    const USDC_USDT_BUILD: &str = r#"{
      "inputMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
      "outputMint": "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
      "inAmount": "100000000",
      "outAmount": "100106158",
      "otherAmountThreshold": "99605628",
      "swapMode": "ExactIn",
      "slippageBps": 50,
      "priceImpactPct": "0",
      "routePlan": [
        {
          "percent": 100,
          "bps": 10000,
          "swapInfo": {
            "ammKey": "GMCJvYGf5Ex2ARiMquaBDqU6iKM8uiEQkB8jCnoNfHpC",
            "label": "GoonFi V2",
            "inputMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "outputMint": "So11111111111111111111111111111111111111112",
            "inAmount": "100000000",
            "outAmount": "1380753603"
          }
        },
        {
          "percent": 29.23,
          "bps": 2923,
          "swapInfo": {
            "ammKey": "FJnaiidSLXFweWkgbinxEHRykVHsnkzDcYbNDR3RF5LN",
            "label": "BisonFi",
            "inputMint": "So11111111111111111111111111111111111111112",
            "outputMint": "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
            "inAmount": "1380753603",
            "outAmount": "100106158"
          }
        }
      ],
      "computeBudgetInstructions": [
        {
          "programId": "ComputeBudget111111111111111111111111111111",
          "accounts": [],
          "data": "A6oNCgAAAAAA"
        }
      ],
      "setupInstructions": [
        {
          "programId": "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
          "accounts": [
            {
              "pubkey": "7KVJjSVfmiHNbEHRSCUxUgxCsjuHfsFTxvKUdFtnPXfy",
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
            "pubkey": "7KVJjSVfmiHNbEHRSCUxUgxCsjuHfsFTxvKUdFtnPXfy",
            "isSigner": true,
            "isWritable": false
          },
          {
            "pubkey": "GMCJvYGf5Ex2ARiMquaBDqU6iKM8uiEQkB8jCnoNfHpC",
            "isSigner": false,
            "isWritable": true
          }
        ],
        "data": "u2T6zDHErxQ="
      },
      "cleanupInstruction": null,
      "otherInstructions": [],
      "tipInstruction": null,
      "addressesByLookupTableAddress": {
        "DttEs7CNMNwtH4gc5cfusPJn3xHvavEt8eAfDtDGTEFc": [
          "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        ],
        "DBmHWCVEGCzZ3zDNr9WzRaMmSqCjcixMh78imXfno9qJ": [
          "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"
        ]
      },
      "blockhashWithMetadata": {
        "blockhash": [1, 2, 3],
        "lastValidBlockHeight": 123
      }
    }"#;

    fn parse(body: &str) -> (QuoteResponse, SwapInstructionsResponse) {
        serde_json::from_str::<BuildResponse>(body)
            .expect("deserialize build response")
            .try_into_sdk_types()
            .expect("map build response")
    }

    #[test]
    fn maps_build_response_quote() {
        let (quote, _) = parse(USDC_USDT_BUILD);

        assert_eq!(
            quote.input_mint.to_string(),
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        );
        assert_eq!(
            quote.output_mint.to_string(),
            "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"
        );
        assert_eq!(quote.in_amount, 100_000_000);
        assert_eq!(quote.out_amount, 100_106_158);
        assert_eq!(quote.other_amount_threshold, 99_605_628);
        assert_eq!(quote.swap_mode, SwapMode::ExactIn);
        assert_eq!(quote.slippage_bps, 50);
        assert_eq!(quote.price_impact_pct, Decimal::ZERO);
        assert_eq!(quote.context_slot, 0);
        assert_eq!(quote.time_taken, 0.0);
        assert!(quote.platform_fee.is_none());

        assert_eq!(quote.route_plan.len(), 2);
        assert_eq!(quote.route_plan[0].percent, 100);
        assert_eq!(quote.route_plan[0].swap_info.label, "GoonFi V2");
        assert_eq!(quote.route_plan[0].swap_info.in_amount, 100_000_000);
        // fractional v2 split rounds onto the u8 `percent`
        assert_eq!(quote.route_plan[1].percent, 29);
        // v2 no longer reports per-hop fees
        assert!(quote.route_plan[0].swap_info.fee_amount.is_none());
        assert!(quote.route_plan[0].swap_info.fee_mint.is_none());
    }

    #[test]
    fn maps_build_response_instructions() {
        let (_, ixs) = parse(USDC_USDT_BUILD);

        assert!(ixs.token_ledger_instruction.is_none());
        assert_eq!(ixs.compute_budget_instructions.len(), 1);
        assert_eq!(
            ixs.compute_budget_instructions[0].program_id.to_string(),
            COMPUTE_BUDGET
        );
        assert_eq!(ixs.setup_instructions.len(), 1);
        assert_eq!(ixs.swap_instruction.program_id.to_string(), JUP_V6);
        assert_eq!(ixs.swap_instruction.accounts.len(), 2);
        assert!(ixs.swap_instruction.accounts[0].is_signer);
        assert!(!ixs.swap_instruction.accounts[0].is_writable);
        assert!(ixs.swap_instruction.accounts[1].is_writable);
        // base64 `u2T6zDHErxQ=` is the v6 `route` discriminator
        assert_eq!(
            ixs.swap_instruction.data,
            vec![0xbb, 0x64, 0xfa, 0xcc, 0x31, 0xc4, 0xaf, 0x14]
        );
        assert!(ixs.cleanup_instruction.is_none());
        assert!(ixs.other_instructions.is_empty());
    }

    #[test]
    fn parses_lookup_table_keys() {
        let (_, ixs) = parse(USDC_USDT_BUILD);

        let mut keys: Vec<String> = ixs
            .address_lookup_table_addresses
            .iter()
            .map(ToString::to_string)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "DBmHWCVEGCzZ3zDNr9WzRaMmSqCjcixMh78imXfno9qJ".to_string(),
                "DttEs7CNMNwtH4gc5cfusPJn3xHvavEt8eAfDtDGTEFc".to_string(),
            ]
        );
    }

    /// A non-null `tipInstruction` must land in `other_instructions` so
    /// `TransactionBuilder::build_jupiter_swap_ixs` refuses the route instead of splicing a
    /// System transfer between `swap_begin`/`swap_end` (which the program rejects).
    #[test]
    fn tip_instruction_folds_into_other_instructions() {
        let body = USDC_USDT_BUILD.replace(
            r#""tipInstruction": null"#,
            r#""tipInstruction": {
              "programId": "11111111111111111111111111111111",
              "accounts": [
                {"pubkey": "7KVJjSVfmiHNbEHRSCUxUgxCsjuHfsFTxvKUdFtnPXfy", "isSigner": true, "isWritable": true},
                {"pubkey": "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5", "isSigner": false, "isWritable": true}
              ],
              "data": "AgAAAECJAAAAAAAA"
            }"#,
        );
        assert_ne!(body, USDC_USDT_BUILD, "fixture substitution applied");

        let (_, ixs) = parse(&body);
        assert_eq!(ixs.other_instructions.len(), 1);
        assert_eq!(
            ixs.other_instructions[0].program_id.to_string(),
            SYSTEM_PROGRAM
        );
    }

    /// `otherInstructions` entries are preserved and the tip is appended after them.
    #[test]
    fn other_instructions_precede_the_tip() {
        let body = USDC_USDT_BUILD
            .replace(
                r#""otherInstructions": []"#,
                r#""otherInstructions": [
                  {"programId": "ComputeBudget111111111111111111111111111111", "accounts": [], "data": "AQ=="}
                ]"#,
            )
            .replace(
                r#""tipInstruction": null"#,
                r#""tipInstruction": {"programId": "11111111111111111111111111111111", "accounts": [], "data": "AQ=="}"#,
            );

        let (_, ixs) = parse(&body);
        let program_ids: Vec<String> = ixs
            .other_instructions
            .iter()
            .map(|ix| ix.program_id.to_string())
            .collect();
        assert_eq!(program_ids, vec![COMPUTE_BUDGET, SYSTEM_PROGRAM]);
    }

    /// v2 sends `priceImpactPct` as a full-scale decimal *string*.
    #[test]
    fn parses_fractional_price_impact() {
        let body = USDC_USDT_BUILD.replace(
            r#""priceImpactPct": "0""#,
            r#""priceImpactPct": "0.0084866618200544905813785251""#,
        );
        let (quote, _) = parse(&body);
        assert_eq!(
            quote.price_impact_pct,
            Decimal::from_str("0.0084866618200544905813785251").unwrap()
        );
    }

    #[test]
    fn maps_exact_out_swap_mode() {
        let body = USDC_USDT_BUILD.replace(r#""swapMode": "ExactIn""#, r#""swapMode": "ExactOut""#);
        let (quote, _) = parse(&body);
        assert_eq!(quote.swap_mode, SwapMode::ExactOut);
    }

    #[test]
    fn maps_cleanup_instruction_when_present() {
        let body = USDC_USDT_BUILD.replace(
            r#""cleanupInstruction": null"#,
            r#""cleanupInstruction": {
              "programId": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
              "accounts": [],
              "data": "CQ=="
            }"#,
        );
        let (_, ixs) = parse(&body);
        assert_eq!(
            ixs.cleanup_instruction.expect("cleanup ix").data,
            vec![0x09]
        );
    }

    #[test]
    fn describes_zod_validation_error() {
        let msg = describe_jupiter_error(
            r#"{"success":false,"error":{"issues":[{"code":"invalid_type","expected":"string",
               "received":"undefined","path":["taker"],"message":"Required"}],"name":"ZodError"}}"#,
        );
        assert!(msg.contains("taker"), "{msg}");
        assert!(msg.contains("Required"), "{msg}");
        assert!(!msg.contains("[object Object]"), "{msg}");
        assert!(!msg.contains("issues"), "{msg}");
    }

    #[test]
    fn describes_rate_limit_error() {
        let msg =
            describe_jupiter_error(r#"{"code":429,"message":"You have exceeded the rate limit"}"#);
        assert!(msg.contains("exceeded the rate limit"), "{msg}");
        assert!(msg.contains("429"), "{msg}");
    }

    #[test]
    fn describes_v1_style_routing_error() {
        let msg = describe_jupiter_error(
            r#"{"error":"Could not find any route","errorCode":"COULD_NOT_FIND_ANY_ROUTE"}"#,
        );
        assert_eq!(msg, "Could not find any route (COULD_NOT_FIND_ANY_ROUTE)");
    }

    #[test]
    fn describes_unparseable_error_body() {
        assert_eq!(
            describe_jupiter_error("<html>502</html>"),
            "<html>502</html>"
        );
        let long = "x".repeat(1000);
        let msg = describe_jupiter_error(&long);
        assert!(msg.ends_with('…'));
        assert_eq!(msg.chars().count(), 513);
    }
}
