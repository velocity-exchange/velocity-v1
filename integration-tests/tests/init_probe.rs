//! Probe: run the real initialize_user_stats path (validator e2e hit a
//! stack access violation there).

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
