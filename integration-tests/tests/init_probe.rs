//! Probes over the real account-creation instructions. Both of these
//! failed only at runtime, on a validator, with the program compiling
//! cleanly — so they are pinned here where a `cargo test` catches them.

use {
    anchor_lang::{InstructionData, ToAccountMetas},
    solana_instruction::AccountMeta,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    velocity_integration_tests::*,
};

#[test]
fn initialize_user_stats_runs() {
    let mut svm = svm();
    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();
    set_state(&mut svm, &admin.pubkey());

    let user_stats =
        Pubkey::find_program_address(&[b"user_stats", admin.pubkey().as_ref()], &velocity_id()).0;
    let ix = solana_instruction::Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeUserStats {
            user_stats,
            state: state_pda(),
            authority: admin.pubkey(),
            payer: admin.pubkey(),
            rent: "SysvarRent111111111111111111111111111111111"
                .parse()
                .unwrap(),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None)
        .into_iter()
        .map(|m| AccountMeta {
            pubkey: m.pubkey,
            is_signer: m.is_signer,
            is_writable: m.is_writable,
        })
        .collect(),
        data: velocity::instruction::InitializeUserStats {}.data(),
    };
    let meta = send(&mut svm, &admin, ix, &[]).unwrap();
    println!("CU: {}", meta.compute_units_consumed);
}

/// `initialize_user` creates the user *and* its relay liquidation-conditions
/// block, both zero-copy and both `init`. Anchor's `load_init` does not
/// write the discriminator until the instruction exits, so any `load_mut`
/// of one of them in between reads a zeroed discriminator and fails — a
/// runtime-only error that a `cargo check` cannot see.
#[test]
fn initialize_user_creates_its_liq_conditions() {
    let mut svm = svm();
    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();
    set_state(&mut svm, &admin.pubkey());

    let system_program: Pubkey = "11111111111111111111111111111111".parse().unwrap();
    let rent: Pubkey = "SysvarRent111111111111111111111111111111111"
        .parse()
        .unwrap();
    let user_stats =
        Pubkey::find_program_address(&[b"user_stats", admin.pubkey().as_ref()], &velocity_id()).0;
    let user = Pubkey::find_program_address(
        &[b"user", admin.pubkey().as_ref(), &0u16.to_le_bytes()],
        &velocity_id(),
    )
    .0;
    let user_conditions =
        Pubkey::find_program_address(&[b"user_conditions", user.as_ref()], &velocity_id()).0;

    let metas = |accounts: Vec<anchor_lang::prelude::AccountMeta>| -> Vec<AccountMeta> {
        accounts
            .into_iter()
            .map(|m| AccountMeta {
                pubkey: m.pubkey,
                is_signer: m.is_signer,
                is_writable: m.is_writable,
            })
            .collect()
    };

    let init_stats = solana_instruction::Instruction {
        program_id: velocity_id(),
        accounts: metas(
            velocity::accounts::InitializeUserStats {
                user_stats,
                state: state_pda(),
                authority: admin.pubkey(),
                payer: admin.pubkey(),
                rent,
                system_program,
            }
            .to_account_metas(None),
        ),
        data: velocity::instruction::InitializeUserStats {}.data(),
    };
    send(&mut svm, &admin, init_stats, &[]).unwrap();

    let init_user = solana_instruction::Instruction {
        program_id: velocity_id(),
        accounts: metas(
            velocity::accounts::InitializeUser {
                user,
                user_stats,
                state: state_pda(),
                authority: admin.pubkey(),
                payer: admin.pubkey(),
                rent,
                system_program,
                user_conditions: Some(user_conditions),
            }
            .to_account_metas(None),
        ),
        data: velocity::instruction::InitializeUser {
            sub_account_id: 0,
            name: [b' '; 32],
        }
        .data(),
    };
    send(&mut svm, &admin, init_user, &[]).unwrap();

    // The block exists, is owned by velocity, and points back at its user.
    let account = svm.get_account(&user_conditions).expect("liq conditions");
    assert_eq!(account.owner, velocity_id());
    let conditions: &velocity::state::user_conditions::UserConditionsV0 = bytemuck::from_bytes(
        &account.data[8..velocity::state::user_conditions::UserConditionsV0::SIZE],
    );
    assert_eq!(conditions.user, user);
    // `init_block` ran: the header is a valid, all-inactive relay block.
    assert_eq!(&conditions.block()[..8], b"RELAY-V0");
}
