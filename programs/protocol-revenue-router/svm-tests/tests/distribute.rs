#![allow(deprecated)]

//! litesvm integration tests for the router's `distribute`, run against the
//! real dfx-redemption program so the CPI, the cap clamp and the tier ladder
//! are exercised end to end.
//!
//! Build the router first: `bash deploy-scripts/build-sbf.sh test protocol-revenue-router`
//! (the default build enables the mainnet init gate, which these tests do not
//! sign for). The redemption program loads from `fixtures/dfx_redemption.so`,
//! or from `DFX_REDEMPTION_SO` when set; see the README to refresh it.

use {
    anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas},
    litesvm::{
        types::{FailedTransactionMetadata, TransactionMetadata},
        LiteSVM,
    },
    protocol_revenue_router::{
        accounts as router_accounts, dfx_redemption, instruction as router_args,
        state::{RouterConfig, Tier, ROUTER_CONFIG_SEED, SECONDS_PER_DAY},
        ID as ROUTER_ID,
    },
    solana_clock::Clock,
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_program_pack::Pack,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_transaction::Transaction,
    spl_associated_token_account::{
        get_associated_token_address, instruction::create_associated_token_account_idempotent,
    },
    spl_token::{instruction as token_ix, state::Mint},
};

const ROUTER_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../target/deploy/protocol_revenue_router.so"
);
const FIXTURE_REDEMPTION_SO: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/dfx_redemption.so");

const USDT: u64 = 1_000_000;
const TOTAL_EXPLOITED: u64 = 20_000 * USDT;
const REDEMPTION_THRESHOLD: u64 = 5_000 * USDT;

type TxResult = Result<TransactionMetadata, FailedTransactionMetadata>;

fn ladder() -> Vec<Tier> {
    vec![
        Tier {
            threshold: 0,
            pool_bps: 6000,
        },
        Tier {
            threshold: 30_000_000_000,
            pool_bps: 7000,
        },
        Tier {
            threshold: 100_000_000_000,
            pool_bps: 9000,
        },
    ]
}

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    admin: Keypair,
    cranker: Keypair,
    treasury: Pubkey,
    usdt_mint: Pubkey,
    router_config: Pubkey,
    router_ata: Pubkey,
    redemption_config: Pubkey,
    redemption_ledger: Pubkey,
    redemption_vault: Pubkey,
}

fn setup() -> Env {
    let mut env = setup_without_router();
    env.initialize_router(ladder()).expect("router initialize");
    env
}

/// Both programs loaded and the redemption side initialized, router not yet.
fn setup_without_router() -> Env {
    let redemption_so =
        std::env::var("DFX_REDEMPTION_SO").unwrap_or_else(|_| FIXTURE_REDEMPTION_SO.to_string());

    let mut svm = LiteSVM::new();
    svm.add_program_from_file(ROUTER_ID, ROUTER_SO)
        .expect("load protocol_revenue_router.so (run `bash deploy-scripts/build-sbf.sh test protocol-revenue-router`)");
    svm.add_program_from_file(dfx_redemption::ID, &redemption_so)
        .unwrap_or_else(|e| panic!("load {redemption_so}: {e:?}"));

    let payer = Keypair::new();
    let admin = Keypair::new();
    let cranker = Keypair::new();
    let treasury = Pubkey::new_unique();
    for kp in [&payer, &admin, &cranker] {
        svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
    }

    let usdt_mint_kp = Keypair::new();
    let dfx_mint_kp = Keypair::new();
    let usdt_mint = usdt_mint_kp.pubkey();

    let (router_config, _) = Pubkey::find_program_address(&[ROUTER_CONFIG_SEED], &ROUTER_ID);
    let (redemption_config, _) = Pubkey::find_program_address(&[b"config"], &dfx_redemption::ID);
    let (redemption_ledger, _) =
        Pubkey::find_program_address(&[b"contribution_ledger"], &dfx_redemption::ID);

    let mut env = Env {
        svm,
        payer,
        admin,
        cranker,
        treasury,
        usdt_mint,
        router_config,
        router_ata: get_associated_token_address(&router_config, &usdt_mint),
        redemption_config,
        redemption_ledger,
        redemption_vault: get_associated_token_address(&redemption_config, &usdt_mint),
    };

    let mint_authority = env.admin.pubkey();
    env.create_mint(&usdt_mint_kp, &mint_authority);
    env.create_mint(&dfx_mint_kp, &mint_authority);

    env.initialize_redemption(dfx_mint_kp.pubkey())
        .expect("redemption initialize_config");
    env.initialize_ledger()
        .expect("initialize_contribution_ledger");
    env
}

impl Env {
    fn initialize_redemption(&mut self, dfx_mint: Pubkey) -> TxResult {
        let accounts = dfx_redemption::client::accounts::InitializeConfig {
            config: self.redemption_config,
            usdt_vault: self.redemption_vault,
            dfx_mint,
            usdt_mint: self.usdt_mint,
            admin: self.admin.pubkey(),
            system_program: system_program::ID,
            token_program: spl_token::ID,
            associated_token_program: spl_associated_token_account::ID,
        };
        let data = dfx_redemption::client::args::InitializeConfig {
            total_exploited_amount: TOTAL_EXPLOITED,
            redemption_threshold: REDEMPTION_THRESHOLD,
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix(dfx_redemption::ID, accounts, data)], &[&admin])
    }

    fn initialize_ledger(&mut self) -> TxResult {
        let accounts = dfx_redemption::client::accounts::InitializeContributionLedger {
            config: self.redemption_config,
            contribution_ledger: self.redemption_ledger,
            admin: self.admin.pubkey(),
            system_program: system_program::ID,
        };
        let data = dfx_redemption::client::args::InitializeContributionLedger {
            router_program: ROUTER_ID,
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix(dfx_redemption::ID, accounts, data)], &[&admin])
    }

    fn initialize_router(&mut self, tiers: Vec<Tier>) -> TxResult {
        let usdt_mint = self.usdt_mint;
        self.initialize_router_with_mint(tiers, usdt_mint)
    }

    fn initialize_router_with_mint(&mut self, tiers: Vec<Tier>, usdt_mint: Pubkey) -> TxResult {
        let accounts = router_accounts::Initialize {
            config: self.router_config,
            usdt_mint,
            redemption_config: self.redemption_config,
            payer: self.payer.pubkey(),
            system_program: system_program::ID,
        };
        let data = router_args::Initialize {
            admin: self.admin.pubkey(),
            cranker: self.cranker.pubkey(),
            treasury: self.treasury,
            tiers,
        };
        self.send(&[ix(ROUTER_ID, accounts, data)], &[])
    }

    fn set_tiers(&mut self, tiers: Vec<Tier>) -> TxResult {
        let accounts = router_accounts::AdminUpdate {
            config: self.router_config,
            admin: self.admin.pubkey(),
        };
        let admin = self.admin.insecure_clone();
        self.send(
            &[ix(ROUTER_ID, accounts, router_args::SetTiers { tiers })],
            &[&admin],
        )
    }

    fn set_treasury(&mut self, treasury: Pubkey) -> TxResult {
        let accounts = router_accounts::AdminUpdate {
            config: self.router_config,
            admin: self.admin.pubkey(),
        };
        let admin = self.admin.insecure_clone();
        self.send(
            &[ix(
                ROUTER_ID,
                accounts,
                router_args::SetTreasury {
                    new_treasury: treasury,
                },
            )],
            &[&admin],
        )
    }

    fn distribute(&mut self, cranker: &Keypair) -> TxResult {
        let payer = self.payer.pubkey();
        self.distribute_with_payer(cranker, payer)
    }

    fn distribute_with_payer(&mut self, cranker: &Keypair, payer: Pubkey) -> TxResult {
        let accounts = router_accounts::Distribute {
            config: self.router_config,
            cranker: cranker.pubkey(),
            usdt_mint: self.usdt_mint,
            router_ata: self.router_ata,
            treasury: self.treasury,
            treasury_ata: get_associated_token_address(&self.treasury, &self.usdt_mint),
            payer,
            redemption_config: self.redemption_config,
            redemption_ledger: self.redemption_ledger,
            redemption_vault: self.redemption_vault,
            dfx_redemption_program: dfx_redemption::ID,
            token_program: spl_token::ID,
            associated_token_program: spl_associated_token_account::ID,
            system_program: system_program::ID,
        };
        let signer = cranker.insecure_clone();
        self.send(
            &[ix(ROUTER_ID, accounts, router_args::Distribute {})],
            &[&signer],
        )
    }

    /// Fund the router's ATA, creating it the way Velocity's withdrawal would.
    fn fund_router(&mut self, amount: u64) {
        let create = create_associated_token_account_idempotent(
            &self.payer.pubkey(),
            &self.router_config,
            &self.usdt_mint,
            &spl_token::ID,
        );
        let mint_to = token_ix::mint_to(
            &spl_token::ID,
            &self.usdt_mint,
            &self.router_ata,
            &self.admin.pubkey(),
            &[],
            amount,
        )
        .unwrap();
        let admin = self.admin.insecure_clone();
        self.send(&[create, mint_to], &[&admin])
            .expect("fund router ata");
    }

    /// Move the clock `days` forward so a new period can start.
    fn warp_days(&mut self, days: i64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp += days * SECONDS_PER_DAY;
        clock.slot += 1;
        self.svm.set_sysvar(&clock);
        self.svm.warp_to_slot(clock.slot);
    }

    fn router_config(&self) -> RouterConfig {
        let acct = self.svm.get_account(&self.router_config).expect("config");
        RouterConfig::try_deserialize(&mut acct.data.as_slice()).expect("decode RouterConfig")
    }

    fn balance(&self, token_account: &Pubkey) -> u64 {
        match self.svm.get_account(token_account) {
            Some(acct) if !acct.data.is_empty() => {
                spl_token::state::Account::unpack(&acct.data)
                    .expect("decode token account")
                    .amount
            }
            _ => 0,
        }
    }

    fn treasury_balance(&self) -> u64 {
        self.balance(&get_associated_token_address(
            &self.treasury,
            &self.usdt_mint,
        ))
    }

    fn create_mint(&mut self, mint: &Keypair, authority: &Pubkey) {
        let rent = self.svm.minimum_balance_for_rent_exemption(Mint::LEN);
        let create = system_instruction::create_account(
            &self.payer.pubkey(),
            &mint.pubkey(),
            rent,
            Mint::LEN as u64,
            &spl_token::ID,
        );
        let init =
            token_ix::initialize_mint(&spl_token::ID, &mint.pubkey(), authority, None, 6).unwrap();
        let mint_kp = mint.insecure_clone();
        self.send(&[create, init], &[&mint_kp])
            .expect("create mint");
    }

    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> TxResult {
        self.svm.expire_blockhash();
        let blockhash = self.svm.latest_blockhash();
        let mut all: Vec<&Keypair> = vec![&self.payer];
        all.extend(signers);
        let tx =
            Transaction::new_signed_with_payer(ixs, Some(&self.payer.pubkey()), &all, blockhash);
        self.svm.send_transaction(tx)
    }
}

fn ix(
    program_id: Pubkey,
    accounts: impl ToAccountMetas,
    data: impl InstructionData,
) -> Instruction {
    Instruction {
        program_id,
        accounts: accounts.to_account_metas(None),
        data: data.data(),
    }
}

fn assert_error(result: TxResult, needle: &str) {
    let err = result.expect_err("expected the transaction to fail");
    let logs = err.meta.logs.join("\n");
    assert!(
        logs.contains(needle),
        "expected logs to contain {needle:?}, got:\n{logs}"
    );
}

#[test]
fn distribute_clamps_the_pool_share_to_the_remaining_cap() {
    let mut env = setup();

    env.fund_router(120_000 * USDT);
    let cranker = env.cranker.insecure_clone();
    env.distribute(&cranker).expect("distribute");

    // The ladder alone would send 85k; the pool only has 20k of room left.
    assert_eq!(env.balance(&env.redemption_vault), TOTAL_EXPLOITED);
    assert_eq!(env.treasury_balance(), 100_000 * USDT);
    assert_eq!(env.balance(&env.router_ata), 0);

    let config = env.router_config();
    assert_eq!(config.period_fees, (120_000 * USDT) as u128);
    assert_eq!(config.lifetime_to_pool, TOTAL_EXPLOITED as u128);
    assert_eq!(
        config.lifetime_to_treasury,
        (120_000 * USDT - TOTAL_EXPLOITED) as u128
    );
}

#[test]
fn distribute_uses_the_first_tier_when_cap_room_is_ample() {
    let mut env = setup();

    env.fund_router(10_000 * USDT);
    let cranker = env.cranker.insecure_clone();
    env.distribute(&cranker).expect("distribute");

    assert_eq!(env.balance(&env.redemption_vault), 6_000 * USDT);
    assert_eq!(env.treasury_balance(), 4_000 * USDT);
}

#[test]
fn distribute_rejects_an_unknown_signer() {
    let mut env = setup();

    env.fund_router(10_000 * USDT);
    let stranger = Keypair::new();
    env.svm.airdrop(&stranger.pubkey(), 1_000_000_000).unwrap();
    assert_error(env.distribute(&stranger), "Unauthorized");
}

#[test]
fn tiers_are_locked_for_the_rest_of_the_distributing_day() {
    let mut env = setup();

    env.fund_router(10_000 * USDT);
    let cranker = env.cranker.insecure_clone();
    env.distribute(&cranker).expect("distribute");

    assert_error(
        env.set_tiers(vec![Tier {
            threshold: 0,
            pool_bps: 5000,
        }]),
        "TiersLockedForPeriod",
    );

    env.warp_days(1);
    env.set_tiers(vec![Tier {
        threshold: 0,
        pool_bps: 5000,
    }])
    .expect("set_tiers on a fresh day");
    assert_eq!(env.router_config().tier_count, 1);
}

#[test]
fn the_cranker_may_also_be_the_payer() {
    let mut env = setup();

    env.fund_router(10_000 * USDT);
    let cranker = env.cranker.insecure_clone();
    let payer = cranker.pubkey();
    env.distribute_with_payer(&cranker, payer)
        .expect("distribute with cranker as payer");

    assert_eq!(env.balance(&env.redemption_vault), 6_000 * USDT);
}

#[test]
fn set_treasury_rejects_keys_whose_ata_would_alias_a_leg() {
    let mut env = setup();

    let router_config = env.router_config;
    assert_error(env.set_treasury(router_config), "InvalidTreasury");
    let redemption_config = env.redemption_config;
    assert_error(env.set_treasury(redemption_config), "InvalidTreasury");

    let fresh = Pubkey::new_unique();
    env.set_treasury(fresh).expect("set_treasury to a wallet");
    assert_eq!(env.router_config().treasury, fresh);
}

#[test]
fn distribute_pays_the_treasury_before_the_redemption_cpi() {
    let mut env = setup();

    env.fund_router(10_000 * USDT);
    let cranker = env.cranker.insecure_clone();
    let logs = env.distribute(&cranker).expect("distribute").logs;

    // The router ATA must hold only the pool share by the time the external
    // program is given signer authority over it.
    let position = |needle: &str| {
        logs.iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no log line containing {needle:?}:\n{}", logs.join("\n")))
    };
    let treasury_transfer = position(&format!("Program {} invoke [2]", spl_token::ID));
    let redemption_cpi = position(&format!("Program {} invoke [2]", dfx_redemption::ID));
    assert!(
        treasury_transfer < redemption_cpi,
        "treasury transfer (log {treasury_transfer}) must precede the redemption CPI (log {redemption_cpi})"
    );
    assert_eq!(env.treasury_balance(), 4_000 * USDT);
    assert_eq!(env.balance(&env.redemption_vault), 6_000 * USDT);
}

#[test]
fn initialize_rejects_a_mint_other_than_the_redemption_mint() {
    let mut env = setup_without_router();

    let other_mint = Keypair::new();
    let authority = env.admin.pubkey();
    env.create_mint(&other_mint, &authority);
    assert_error(
        env.initialize_router_with_mint(ladder(), other_mint.pubkey()),
        "UsdtMintMismatch",
    );

    env.initialize_router(ladder())
        .expect("initialize with the redemption mint");
    assert_eq!(env.router_config().usdt_mint, env.usdt_mint);
}

#[test]
fn distribute_leaves_cap_room_for_usdt_already_in_the_vault() {
    let mut env = setup();

    // 5k lands in the vault outside `contribute`; contribute's pre-sync
    // recognises it first, leaving 15k of the 20k cap for this contribution.
    let stray = 5_000 * USDT;
    let mint_to = token_ix::mint_to(
        &spl_token::ID,
        &env.usdt_mint,
        &env.redemption_vault,
        &env.admin.pubkey(),
        &[],
        stray,
    )
    .unwrap();
    let admin = env.admin.insecure_clone();
    env.send(&[mint_to], &[&admin]).expect("mint into vault");

    env.fund_router(120_000 * USDT);
    let cranker = env.cranker.insecure_clone();
    env.distribute(&cranker).expect("distribute");

    let to_pool = TOTAL_EXPLOITED - stray;
    assert_eq!(env.router_config().lifetime_to_pool, to_pool as u128);
    assert_eq!(env.treasury_balance(), 120_000 * USDT - to_pool);
    assert_eq!(env.balance(&env.redemption_vault), TOTAL_EXPLOITED);
    assert_eq!(env.balance(&env.router_ata), 0);

    let acct = env
        .svm
        .get_account(&env.redemption_config)
        .expect("redemption config");
    let rc = dfx_redemption::accounts::Config::try_deserialize(&mut acct.data.as_slice())
        .expect("decode redemption Config");
    assert_eq!(rc.lifetime_recognized_backing, TOTAL_EXPLOITED);
    assert_eq!(rc.recognized_backing_remaining, TOTAL_EXPLOITED);
}
