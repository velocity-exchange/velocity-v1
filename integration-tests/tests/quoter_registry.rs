//! Registry lifecycle smoke test against the real velocity.so, with clob.so
//! standing in as the (executable) quoter program. Requires both fixtures:
//! `bun run program:build` and `bun run program:build:clob`.

use {
    anchor_lang::{InstructionData, ToAccountMetas},
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    velocity::{
        instructions::{
            InitializeQuoterArgs, QuoterAccountMetaArg, UpdateQuoterAccountsArgs,
            UpdateQuoterConfigArgs,
        },
        state::prop_amm::{QuoterCpiLeg, QuoterType, QuoterV0},
    },
    velocity_integration_tests::*,
};

fn rent_sysvar() -> Pubkey {
    "SysvarRent111111111111111111111111111111111"
        .parse()
        .unwrap()
}

fn system_program() -> Pubkey {
    "11111111111111111111111111111111".parse().unwrap()
}

#[test]
fn quoter_registry_lifecycle() {
    let mut svm = svm();
    let admin = Keypair::new();
    let maker = Keypair::new();
    svm.airdrop(&admin.pubkey(), 10_000_000_000).unwrap();
    svm.airdrop(&maker.pubkey(), 10_000_000_000).unwrap();

    set_state(&mut svm, &admin.pubkey());
    set_perp_market(&mut svm, 0);
    let user = Pubkey::new_unique();
    set_user(&mut svm, user, &maker.pubkey());

    let quoter = quoter_pda(0, &clob_id(), &user);
    let response_account = Pubkey::new_unique();

    // Creation is consent: a Custom quoter must be created by the quoted
    // user's authority (clob.so is the executable quoter program).
    let init = |authority: Pubkey| Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
            state: state_pda(),
            payer: authority,
            authority,
            quoter,
            perp_market: perp_market_pda(0),
            quoter_program: clob_id(),
            user,
            rent: rent_sysvar(),
            system_program: system_program(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoter {
            args: InitializeQuoterArgs {
                market_index: 0,
                quoter_type: QuoterType::Custom,
                response_account,
                quote_v0_discriminator: [1; 8],
                quote_l3_v0_discriminator: [0; 8],
                execute_v0_discriminator: [2; 8],
            },
        }
        .data(),
    };
    assert!(send(&mut svm, &admin, init(admin.pubkey()), &[]).is_err());
    send(&mut svm, &maker, init(maker.pubkey()), &[]).unwrap();
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert_eq!(entry.authority.to_bytes(), maker.pubkey().to_bytes());
    assert_eq!(entry.program_id.to_bytes(), clob_id().to_bytes());
    // Born with the maker's switch on but unapproved — nothing can fill.
    assert!(entry.is_active && !entry.is_approved);

    // Approval is rejected while the account lists are empty.
    let approve = |as_admin: Pubkey, approved: bool| Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: as_admin,
            state: state_pda(),
            quoter,
            quoter_program: clob_id(),
            quoter_program_data: None,
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved { approved }.data(),
    };
    assert!(send(&mut svm, &admin, approve(admin.pubkey(), true), &[]).is_err());

    // Register the CPI account lists (both legs include the response account).
    for leg in [QuoterCpiLeg::Quote, QuoterCpiLeg::Execute] {
        let ix = Instruction {
            program_id: velocity_id(),
            accounts: velocity::accounts::UpdateQuoterAccounts {
                authority: maker.pubkey(),
                quoter,
            }
            .to_account_metas(None),
            data: velocity::instruction::UpdateQuoterAccounts {
                args: UpdateQuoterAccountsArgs {
                    leg,
                    index: 0,
                    metas: vec![QuoterAccountMetaArg {
                        pubkey: response_account,
                        is_writable: true,
                    }],
                },
            }
            .data(),
        };
        send(&mut svm, &maker, ix, &[]).unwrap();
    }
    // A signer that isn't the entry's authority can't touch the lists.
    let bad_ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterAccounts {
            authority: admin.pubkey(),
            quoter,
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterAccounts {
            args: UpdateQuoterAccountsArgs {
                leg: QuoterCpiLeg::Quote,
                index: 0,
                metas: vec![],
            },
        }
        .data(),
    };
    assert!(send(&mut svm, &admin, bad_ix, &[]).is_err());

    // Admin approves; a non-admin claiming the role can't.
    assert!(send(&mut svm, &maker, approve(maker.pubkey(), true), &[]).is_err());
    send(&mut svm, &admin, approve(admin.pubkey(), true), &[]).unwrap();
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert!(entry.is_active && entry.is_approved);

    // The maker's kill switch always works and doesn't touch approval.
    let set_active = |authority: Pubkey, active: bool| Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterActive { authority, quoter }
            .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterActive { active }.data(),
    };
    assert!(send(&mut svm, &admin, set_active(admin.pubkey(), false), &[]).is_err());
    send(&mut svm, &maker, set_active(maker.pubkey(), false), &[]).unwrap();
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert!(!entry.is_active && entry.is_approved);
    send(&mut svm, &maker, set_active(maker.pubkey(), true), &[]).unwrap();

    // Any config change clears approval for admin re-vetting.
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterConfig {
            authority: maker.pubkey(),
            quoter,
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterConfig {
            args: UpdateQuoterConfigArgs {
                response_account: Some(Pubkey::new_unique().to_bytes().into()),
                quote_v0_discriminator: None,
                quote_l3_v0_discriminator: None,
                execute_v0_discriminator: None,
            },
        }
        .data(),
    };
    send(&mut svm, &maker, ix, &[]).unwrap();
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert!(entry.is_active && !entry.is_approved);
}

/// The entry's *type* is the whole boundary now: a Custom entry may only
/// settle for the one account it registered, while a book settles for
/// whoever a fill carries. So typing an entry as a book is the admin's call,
/// and a maker asking for it is refused.
#[test]
fn only_the_admin_may_register_a_book() {
    let mut svm = svm();
    let admin = Keypair::new();
    let maker = Keypair::new();
    for key in [&admin, &maker] {
        svm.airdrop(&key.pubkey(), 10_000_000_000).unwrap();
    }
    set_state(&mut svm, &admin.pubkey());
    set_perp_market(&mut svm, 0);
    let user = Pubkey::new_unique();
    set_user(&mut svm, user, &maker.pubkey());

    let quoter = quoter_pda(0, &clob_id(), &user);
    let init = |authority: Pubkey| Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
            state: state_pda(),
            payer: authority,
            authority,
            quoter,
            perp_market: perp_market_pda(0),
            quoter_program: clob_id(),
            user,
            rent: rent_sysvar(),
            system_program: system_program(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoter {
            args: InitializeQuoterArgs {
                market_index: 0,
                quoter_type: QuoterType::Clob,
                response_account: Pubkey::new_unique(),
                quote_v0_discriminator: [1; 8],
                quote_l3_v0_discriminator: [3; 8],
                execute_v0_discriminator: [2; 8],
            },
        }
        .data(),
    };

    assert!(
        send(&mut svm, &maker, init(maker.pubkey()), &[]).is_err(),
        "a maker cannot type its own entry as a book"
    );
    send(&mut svm, &admin, init(admin.pubkey()), &[]).unwrap();

    // Registering the book is the market's designation of it, and the
    // designation is one-way: a second book cannot take the market over, so
    // whoever holds the admin key later cannot point the market at a book
    // that would settle for every user a fill carries.
    let market: velocity::state::perp_market::PerpMarket =
        read_zero_copy(&svm, &perp_market_pda(0));
    assert_eq!(market.clob_quoter, quoter);

    let other_user = Pubkey::new_unique();
    set_user(&mut svm, other_user, &admin.pubkey());
    let second = quoter_pda(0, &clob_id(), &other_user);
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoter {
            state: state_pda(),
            payer: admin.pubkey(),
            authority: admin.pubkey(),
            quoter: second,
            perp_market: perp_market_pda(0),
            quoter_program: clob_id(),
            user: other_user,
            rent: rent_sysvar(),
            system_program: system_program(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoter {
            args: InitializeQuoterArgs {
                market_index: 0,
                quoter_type: QuoterType::Clob,
                response_account: Pubkey::new_unique(),
                quote_v0_discriminator: [1; 8],
                quote_l3_v0_discriminator: [3; 8],
                execute_v0_discriminator: [2; 8],
            },
        }
        .data(),
    };
    assert!(send(&mut svm, &admin, ix, &[]).is_err());
    let market: velocity::state::perp_market::PerpMarket =
        read_zero_copy(&svm, &perp_market_pda(0));
    assert_eq!(market.clob_quoter, quoter, "the market kept its book");
}
