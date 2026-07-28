//! Cross-program router probe: velocity's `probe_router` (anchor-test builds
//! only) quotes three CLOB books through their `QuoterV0` entries, splits
//! the taker size per the priority-tier waterfall (CLOB-typed entry first at
//! a price, customs pro rata), executes each allocation, and enforces
//! at-or-better-than-quote — validating the whole wire protocol and
//! measuring a realistic 3-quoter fill's CPI cost.
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
    InitializeQuoterArgs, ProbeRouterArgs, QuoterAccountMetaArg, UpdateQuoterAccountsArgs,
};
use velocity::state::prop_amm::{Direction, QuoterCpiLeg, QuoterType, QuoterV0};
use velocity_integration_tests::*;

const UNIT: u64 = 1_000_000_000; // one base unit at perp BASE_PRECISION

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
    v.extend_from_slice(&0u32.to_le_bytes()); // default_activation_delay (active same slot)
    v.extend_from_slice(&20u32.to_le_bytes()); // max_activation_delay
    v.extend_from_slice(&2u32.to_le_bytes()); // unknown_user_grace_slots
    v.extend_from_slice(&100u32.to_le_bytes()); // evict_threshold_per_side
    v.extend_from_slice(&128u16.to_le_bytes()); // max_quote_levels
    v.extend_from_slice(&64u16.to_le_bytes()); // max_execute_fills
    v.extend_from_slice(&32u16.to_le_bytes()); // max_execute_users
    v
}

/// Borsh `PlaceOrderArgsV0`: ask at `price`, one unit, Some(0) delay, GTC.
fn place_args(price: u64) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(1u8); // Side::Ask
    v.extend_from_slice(&price.to_le_bytes());
    v.extend_from_slice(&UNIT.to_le_bytes());
    v.extend_from_slice(&[1, 0, 0, 0, 0]); // Some(0u32) activation delay
    v.extend_from_slice(&0i64.to_le_bytes()); // max_ts = 0 (GTC)
    v
}

fn clob_ask_count(svm: &litesvm::LiteSVM, market: &Pubkey) -> u32 {
    let data = svm.get_account(market).unwrap().data;
    u32::from_le_bytes(data[140..144].try_into().unwrap())
}

/// A CLOB book handed to velocity: init, rest 1-unit asks at `prices` under
/// `user`, then patch `place_authority` (offset 40..72) to the velocity
/// signer PDA so the quoter CPI's invoke_signed satisfies it.
fn make_clob_book(
    svm: &mut litesvm::LiteSVM,
    clob_admin: &Keypair,
    user: Pubkey,
    prices: &[u64],
) -> Pubkey {
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
            AccountMeta::new_readonly(clob_admin.pubkey(), false),
            AccountMeta::new(market, false),
        ],
    );
    send(svm, clob_admin, ix, &[]).unwrap();
    for price in prices {
        let ix = clob_ix(
            "place_order_v0",
            place_args(*price),
            vec![
                AccountMeta::new(market, false),
                AccountMeta::new_readonly(clob_admin.pubkey(), true),
                AccountMeta::new_readonly(user, false),
            ],
        );
        send(svm, clob_admin, ix, &[]).unwrap();
    }
    let (velocity_signer, _) = velocity_signer_pda();
    let mut account = svm.get_account(&market).unwrap();
    account.data[40..72].copy_from_slice(velocity_signer.as_ref());
    svm.set_account(market, account).unwrap();
    market
}

/// Register `market` as a quoter for the perp market 0 and approve it.
fn register_quoter(
    svm: &mut litesvm::LiteSVM,
    admin: &Keypair,
    creator: &Keypair,
    quoter_type: QuoterType,
    user: Pubkey,
    market: Pubkey,
) -> Pubkey {
    let (velocity_signer, _) = velocity_signer_pda();
    let quoter = quoter_pda(0, &clob_id(), &user);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
            payer: creator.pubkey(),
            authority: creator.pubkey(),
            quoter,
            perp_market: perp_market_pda(0),
            quoter_program: clob_id(),
            user,
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoter {
            args: InitializeQuoterArgs {
                market_index: 0,
                quoter_type,
                response_account: market,
                quote_v0_discriminator: ix_discriminator("quote_v0"),
                execute_v0_discriminator: ix_discriminator("execute_v0"),
            },
        }
        .data(),
    };
    send(svm, creator, ix, &[]).unwrap();
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
                authority: creator.pubkey(),
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
        send(svm, creator, ix, &[]).unwrap();
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
    send(svm, admin, ix, &[]).unwrap();
    let entry: QuoterV0 = read_zero_copy(svm, &quoter);
    assert!(entry.is_active && entry.is_approved);
    quoter
}

#[test]
fn probe_router_splits_across_three_clob_books() {
    let mut svm = svm();
    let admin = Keypair::new();
    let maker = Keypair::new();
    let clob_admin = Keypair::new();
    for kp in [&admin, &maker, &clob_admin] {
        svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }
    set_state(&mut svm, &admin.pubkey());
    set_perp_market(&mut svm, 0);
    let (velocity_signer, _) = velocity_signer_pda();

    // Three books under three users (distinct users → distinct quoter PDAs).
    // A is the CLOB-typed entry: 2 asks @100 + 3 @101. B and C are
    // Custom-typed: 2 @100 and 4 @100.
    let users: Vec<Pubkey> = (0..3).map(|_| Pubkey::new_unique()).collect();
    for user in &users {
        set_user(&mut svm, *user, &maker.pubkey());
    }
    let market_a = make_clob_book(&mut svm, &clob_admin, users[0], &[100, 100, 101, 101, 101]);
    let market_b = make_clob_book(&mut svm, &clob_admin, users[1], &[100, 100]);
    let market_c = make_clob_book(&mut svm, &clob_admin, users[2], &[100, 100, 100, 100]);

    let quoter_a = register_quoter(
        &mut svm,
        &admin,
        &admin,
        QuoterType::Clob,
        users[0],
        market_a,
    );
    // Custom entries must be created by the quoted user's authority.
    let quoter_b = register_quoter(
        &mut svm,
        &admin,
        &maker,
        QuoterType::Custom,
        users[1],
        market_b,
    );
    let quoter_c = register_quoter(
        &mut svm,
        &admin,
        &maker,
        QuoterType::Custom,
        users[2],
        market_c,
    );

    // Take 6 units long. At price 100: CLOB A's 2 fill first; the remaining
    // 4 split pro rata over B(2)+C(4) at base-precision granularity →
    // floors 1.333…/2.666…, with the 1-lamport floor dust handed to B.
    let probe = |execute: bool| {
        let mut accounts =
            velocity::accounts::ProbeRouter { state: state_pda() }.to_account_metas(None);
        for quoter in [quoter_a, quoter_b, quoter_c] {
            accounts.push(AccountMeta::new_readonly(quoter, false));
        }
        for market in [market_a, market_b, market_c] {
            accounts.push(AccountMeta::new(market, false));
        }
        accounts.push(AccountMeta::new_readonly(velocity_signer, false));
        accounts.push(AccountMeta::new_readonly(clob_id(), false));
        Instruction {
            program_id: velocity_id(),
            accounts,
            data: velocity::instruction::ProbeRouter {
                args: ProbeRouterArgs {
                    direction: Direction::Long,
                    size: 6 * UNIT,
                    users: Some(users.iter().map(|u| u.to_bytes().into()).collect()),
                    quoter_count: 3,
                    execute,
                },
            }
            .data(),
        }
    };

    let meta = send(&mut svm, &admin, probe(false), &[]).unwrap();
    let expect_split =
        |i: usize, base: u64, quote: u64| format!("probe split {i}: base {base} quote {quote}");
    for (i, base, quote) in [
        (0usize, 2 * UNIT, 200u64),
        (1, 1_333_333_334, 133),
        (2, 2_666_666_666, 266),
    ] {
        let needle = expect_split(i, base, quote);
        assert!(
            meta.logs.iter().any(|l| l.contains(&needle)),
            "missing `{needle}` in {:#?}",
            meta.logs
        );
    }
    let quote_cu = meta.compute_units_consumed;

    let meta = send(&mut svm, &admin, probe(true), &[]).unwrap();
    let full_cu = meta.compute_units_consumed;
    // Fills landed where the split said: A 5→3 (its 2 @100 fully removed);
    // B and C each keep one partially-filled order resting (remainders are
    // above min_order_size, so no cull).
    assert_eq!(clob_ask_count(&svm, &market_a), 3);
    assert_eq!(clob_ask_count(&svm, &market_b), 1);
    assert_eq!(clob_ask_count(&svm, &market_c), 2);

    println!("CU — 3-quoter router probe: quote+split {quote_cu}, quote+split+execute {full_cu}",);
}
