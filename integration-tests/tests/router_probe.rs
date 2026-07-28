//! Cross-program quoter CPI probe: velocity's `probe_quoter` (anchor-test
//! builds only) CPIs the CLOB's `quote_v0` + `execute_v0` through the
//! registered `QuoterV0` entry, validating the whole wire protocol —
//! discriminators, borsh/wincode args (incl. the users set), response
//! pointer, velocity-signer `invoke_signed` — and measuring CU.
//!
//! CLOB instructions are built raw (discriminator + hand-encoded borsh) so
//! this workspace doesn't need the anchor-v2 dependency tree.

use anchor_lang::{InstructionData, ToAccountMetas};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use velocity::instructions::{
    InitializeQuoterArgs, ProbeQuoterArgs, QuoterAccountMetaArg, UpdateQuoterAccountsArgs,
};
use velocity::state::prop_amm::{Direction, QuoterCpiLeg, QuoterType, QuoterV0};
use velocity_integration_tests::*;

const ORDER_SIZE: u64 = 1_000_000_000; // 1 base unit at perp BASE_PRECISION
const RESTING_ORDERS: u64 = 50;
const TAKEN_ORDERS: u64 = 30;

/// `[disc][ClobHeaderV0 8352][len u32][pad][OrderNodeV0 x cap]`.
fn clob_market_space(capacity: usize) -> usize {
    let orders_offset = (8 + 8352 + 4usize).next_multiple_of(8);
    orders_offset + capacity * 96
}

fn clob_ix(name: &str, args: Vec<u8>, accounts: Vec<AccountMeta>) -> Instruction {
    let mut data = ix_discriminator(name).to_vec();
    data.extend_from_slice(&args);
    Instruction {
        program_id: clob_id(),
        accounts,
        data,
    }
}

/// Borsh `MarketConfigV0` (field order = struct order in clob state.rs).
fn market_config(market_index: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&market_index.to_le_bytes());
    v.extend_from_slice(&1_000_000_000u64.to_le_bytes()); // base_precision
    v.extend_from_slice(&1u64.to_le_bytes()); // order_tick_size
    v.extend_from_slice(&1u64.to_le_bytes()); // order_step_size
    v.extend_from_slice(&1u64.to_le_bytes()); // min_order_size
    v.extend_from_slice(&0u32.to_le_bytes()); // default_activation_delay (0: active same slot)
    v.extend_from_slice(&20u32.to_le_bytes()); // max_activation_delay
    v.extend_from_slice(&2u32.to_le_bytes()); // unknown_user_grace_slots
    v.extend_from_slice(&100u32.to_le_bytes()); // evict_threshold_per_side
    v.extend_from_slice(&128u16.to_le_bytes()); // max_quote_levels
    v.extend_from_slice(&64u16.to_le_bytes()); // max_execute_fills
    v.extend_from_slice(&32u16.to_le_bytes()); // max_execute_users
    v
}

/// Borsh `PlaceOrderArgsV0`: side, price, size, Some(0) delay, gtc.
fn place_args(ask: bool, price: u64, size: u64) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(ask as u8); // Side enum: Bid = 0, Ask = 1
    v.extend_from_slice(&price.to_le_bytes());
    v.extend_from_slice(&size.to_le_bytes());
    v.extend_from_slice(&[1, 0, 0, 0, 0]); // Some(0u32) activation delay
    v.extend_from_slice(&0i64.to_le_bytes()); // max_ts = 0 (GTC)
    v
}

fn clob_ask_count(svm: &litesvm::LiteSVM, market: &Pubkey) -> u32 {
    let data = svm.get_account(market).unwrap().data;
    u32::from_le_bytes(data[140..144].try_into().unwrap())
}

#[test]
fn probe_quoter_cpis_the_clob() {
    let mut svm = svm();
    let admin = Keypair::new();
    let maker = Keypair::new();
    let clob_admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 10_000_000_000).unwrap();
    svm.airdrop(&maker.pubkey(), 10_000_000_000).unwrap();
    svm.airdrop(&clob_admin.pubkey(), 10_000_000_000).unwrap();

    set_state(&mut svm, &admin.pubkey());
    set_perp_market(&mut svm, 0);
    let maker_user = Pubkey::new_unique();
    set_user(&mut svm, maker_user, &maker.pubkey());
    let (velocity_signer, _) = velocity_signer_pda();

    // --- CLOB market: create zeroed account, initialize, rest 50 asks. ---
    let market = Pubkey::new_unique();
    svm.set_account(
        market,
        Account {
            lamports: 10_000_000_000,
            data: vec![0u8; clob_market_space(128)],
            owner: clob_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    let ix = clob_ix(
        "initialize_market_v0",
        market_config(0),
        vec![
            AccountMeta::new_readonly(clob_admin.pubkey(), true),
            AccountMeta::new_readonly(clob_admin.pubkey(), false), // place_authority (patched below)
            AccountMeta::new(market, false),
        ],
    );
    send(&mut svm, &clob_admin, ix, &[]).unwrap();

    for i in 0..RESTING_ORDERS {
        let ix = clob_ix(
            "place_order_v0",
            place_args(true, 100 + i, ORDER_SIZE),
            vec![
                AccountMeta::new(market, false),
                AccountMeta::new_readonly(clob_admin.pubkey(), true),
                AccountMeta::new_readonly(maker_user, false),
            ],
        );
        send(&mut svm, &clob_admin, ix, &[]).unwrap();
    }
    assert_eq!(clob_ask_count(&svm, &market), RESTING_ORDERS as u32);

    // Hand the book to velocity: patch `place_authority` (offset 40..72:
    // disc 8 + authority 32) to the velocity signer PDA, which the quoter
    // CPI invoke_signs as. Avoids depending on the clob crate for the
    // update_market ix.
    let mut account = svm.get_account(&market).unwrap();
    account.data[40..72].copy_from_slice(velocity_signer.as_ref());
    svm.set_account(market, account).unwrap();

    // --- Register + approve the CLOB as a quoter. ---
    let quoter = quoter_pda(0, &clob_id(), &maker_user);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
            payer: admin.pubkey(),
            authority: admin.pubkey(),
            quoter,
            perp_market: perp_market_pda(0),
            quoter_program: clob_id(),
            user: maker_user,
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoter {
            args: InitializeQuoterArgs {
                market_index: 0,
                quoter_type: QuoterType::Clob,
                response_account: market,
                quote_v0_discriminator: ix_discriminator("quote_v0"),
                execute_v0_discriminator: ix_discriminator("execute_v0"),
            },
        }
        .data(),
    };
    send(&mut svm, &admin, ix, &[]).unwrap();
    for (leg, metas) in [
        (
            QuoterCpiLeg::Quote,
            vec![QuoterAccountMetaArg {
                pubkey: market,
                is_writable: true,
            }],
        ),
        (
            QuoterCpiLeg::Execute,
            vec![
                QuoterAccountMetaArg {
                    pubkey: market,
                    is_writable: true,
                },
                QuoterAccountMetaArg {
                    pubkey: velocity_signer,
                    is_writable: false,
                },
            ],
        ),
    ] {
        let ix = Instruction {
            program_id: velocity_id(),
            accounts: velocity::accounts::UpdateQuoterAccounts {
                authority: admin.pubkey(),
                quoter,
            }
            .to_account_metas(None),
            data: velocity::instruction::UpdateQuoterAccounts {
                args: UpdateQuoterAccountsArgs {
                    leg,
                    index: 0,
                    metas,
                },
            }
            .data(),
        };
        send(&mut svm, &admin, ix, &[]).unwrap();
    }
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: admin.pubkey(),
            state: state_pda(),
            quoter,
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved { approved: true }.data(),
    };
    send(&mut svm, &admin, ix, &[]).unwrap();
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert!(entry.is_active && entry.is_approved);

    // --- Probe: quote leg, then quote + execute. ---
    let probe = |execute: bool| {
        let mut accounts = velocity::accounts::ProbeQuoter {
            state: state_pda(),
            quoter,
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new(market, false));
        accounts.push(AccountMeta::new_readonly(velocity_signer, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::ProbeQuoter {
                args: ProbeQuoterArgs {
                    direction: Direction::Long,
                    size: TAKEN_ORDERS * ORDER_SIZE,
                    users: Some(vec![maker_user.to_bytes().into()]),
                    execute,
                },
            }
            .data(),
        }
    };

    let meta = send(&mut svm, &admin, probe(false), &[]).unwrap();
    assert!(meta
        .logs
        .iter()
        .any(|l| l.contains(&format!("probe quote: {TAKEN_ORDERS} levels"))));
    let quote_cu = meta.compute_units_consumed;

    let meta = send(&mut svm, &admin, probe(true), &[]).unwrap();
    assert!(meta
        .logs
        .iter()
        .any(|l| l.contains("probe execute: 1 balance changes, 0 cancelled")));
    let full_cu = meta.compute_units_consumed;
    assert_eq!(
        clob_ask_count(&svm, &market),
        (RESTING_ORDERS - TAKEN_ORDERS) as u32
    );

    println!(
        "CU — quoter CPI round-trip over {TAKEN_ORDERS} CLOB orders: quote only {quote_cu}, quote+execute {full_cu}",
    );
}
