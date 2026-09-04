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
            InitializeQuoterArgs, InitializeQuoterSlabArgs, QuoterAccountMetaArg,
            UpdateQuoterAccountsArgs, UpdateQuoterActiveArgs, UpdateQuoterApprovedArgs,
            UpdateQuoterConfigArgs,
        },
        state::prop_amm::{QuoterSlabV0, QuoterSlotV0, QuoterType, QuoterV0},
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

fn init_quoter_ix(
    authority: Pubkey,
    quoter: Pubkey,
    user: Pubkey,
    quoter_type: QuoterType,
    response_account: Pubkey,
) -> Instruction {
    Instruction {
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
                quoter_type,
                response_account,
                quote_v0_discriminator: [1; 8],
                quote_l3_v0_discriminator: [0; 8],
                execute_v0_discriminator: [2; 8],
            },
        }
        .data(),
    }
}

fn set_accounts_ix(authority: Pubkey, quoter: Pubkey, response_account: Pubkey) -> Instruction {
    Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterAccounts { authority, quoter }
            .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterAccounts {
            args: UpdateQuoterAccountsArgs {
                metas: vec![QuoterAccountMetaArg {
                    pubkey: response_account,
                    is_writable: true,
                }],
                quote_indexes: vec![0],
                execute_indexes: vec![0],
            },
        }
        .data(),
    }
}

fn approve_ix(as_admin: Pubkey, quoter: Pubkey, approved: bool) -> Instruction {
    Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterApproved {
            admin: as_admin,
            state: state_pda(),
            quoter,
            quoter_slab: quoter_slab_pda(0),
            quoter_program: clob_id(),
            quoter_program_data: Some(program_data_pda(&clob_id())),
            system_program: "11111111111111111111111111111111".parse().unwrap(),
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterApproved {
            args: UpdateQuoterApprovedArgs { approved },
        }
        .data(),
    }
}

fn slab_ix(payer: Pubkey) -> Instruction {
    Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::InitializeQuoterSlab {
            payer,
            perp_market: perp_market_pda(0),
            quoter_slab: quoter_slab_pda(0),
            rent: rent_sysvar(),
            system_program: system_program(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoterSlab {
            args: InitializeQuoterSlabArgs { market_index: 0 },
        }
        .data(),
    }
}

fn slab_capacity(svm: &litesvm::LiteSVM) -> u16 {
    let slab: QuoterSlabV0 = read_zero_copy(svm, &quoter_slab_pda(0));
    slab.capacity
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
    let init = |authority: Pubkey| {
        init_quoter_ix(
            authority,
            quoter,
            user,
            QuoterType::Custom,
            response_account,
        )
    };
    assert!(send(&mut svm, &admin, init(admin.pubkey()), &[]).is_err());
    send(&mut svm, &maker, init(maker.pubkey()), &[]).unwrap();
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert_eq!(entry.config.authority.to_bytes(), maker.pubkey().to_bytes());
    assert_eq!(entry.config.program_id.to_bytes(), clob_id().to_bytes());
    // Born with the maker's switch on; nothing fills until the admin copies
    // the config into the market's slab.
    assert!(entry.config.is_active);

    // Approval needs the slab, and the slab does not exist yet.
    assert!(send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, true),
        &[]
    )
    .is_err());

    // Slab creation is permissionless. A slab is born at one slot — slot 0,
    // the book's, vacant — and approval grows it to fit each further quoter.
    send(&mut svm, &maker, slab_ix(maker.pubkey()), &[]).unwrap();
    assert_eq!(slab_capacity(&svm), 1);
    assert!(read_slab_slot(&svm, 0, 0).is_vacant());

    // Approval is rejected while the account list is empty.
    assert!(send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, true),
        &[]
    )
    .is_err());

    // Register the CPI account list (both legs forward the response account).
    // A signer that is not the entry's authority cannot touch it.
    assert!(send(
        &mut svm,
        &admin,
        set_accounts_ix(admin.pubkey(), quoter, response_account),
        &[]
    )
    .is_err());
    send(
        &mut svm,
        &maker,
        set_accounts_ix(maker.pubkey(), quoter, response_account),
        &[],
    )
    .unwrap();

    // Admin approves; a non-admin claiming the role can't. The copy lands in
    // slot 1 — slot 0 is reserved for the market's book — and the approval
    // grows the slab to fit it.
    assert!(send(
        &mut svm,
        &maker,
        approve_ix(maker.pubkey(), quoter, true),
        &[]
    )
    .is_err());
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, true),
        &[],
    )
    .unwrap();
    assert_eq!(slab_capacity(&svm), 2);
    assert!(read_slab_slot(&svm, 0, 0).is_vacant());
    let slot: QuoterSlotV0 = read_slab_slot(&svm, 0, 1);
    assert_eq!(slot.entry.to_bytes(), quoter.to_bytes());
    assert!(slot.quotes());
    assert_eq!(
        slot.config.response_account.to_bytes(),
        response_account.to_bytes()
    );
    // Approval records the slot the program was deployed at, so a reader can
    // see a later upgrade. The fixture's program-data account carries slot
    // zero, which is also what a fresh slot holds — so this pins that the
    // write happened rather than the value it wrote.
    assert_eq!(slot.config.approved_program_slot, 0);

    // The maker's kill switch writes through to the live copy when the slab
    // is passed, and only to staging when it is not.
    let set_active = |authority: Pubkey, active: bool, slab: Option<Pubkey>| Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterActive {
            authority,
            quoter,
            quoter_slab: slab,
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterActive {
            args: UpdateQuoterActiveArgs { active },
        }
        .data(),
    };
    assert!(send(
        &mut svm,
        &admin,
        set_active(admin.pubkey(), false, Some(quoter_slab_pda(0))),
        &[]
    )
    .is_err());
    send(
        &mut svm,
        &maker,
        set_active(maker.pubkey(), false, None),
        &[],
    )
    .unwrap();
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert!(!entry.config.is_active);
    assert!(
        read_slab_slot(&svm, 0, 1).quotes(),
        "without the slab the live copy keeps serving"
    );
    send(
        &mut svm,
        &maker,
        set_active(maker.pubkey(), false, Some(quoter_slab_pda(0))),
        &[],
    )
    .unwrap();
    assert!(
        !read_slab_slot(&svm, 0, 1).quotes(),
        "with the slab the kill lands on the live copy at once"
    );
    send(
        &mut svm,
        &maker,
        set_active(maker.pubkey(), true, Some(quoter_slab_pda(0))),
        &[],
    )
    .unwrap();
    assert!(read_slab_slot(&svm, 0, 1).quotes());

    // A staging config edit does not reach the live copy until the admin
    // copies it in again.
    let new_response = Pubkey::new_unique();
    let ix = Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::UpdateQuoterConfig {
            authority: maker.pubkey(),
            quoter,
        }
        .to_account_metas(None),
        data: velocity::instruction::UpdateQuoterConfig {
            args: UpdateQuoterConfigArgs {
                response_account: Some(new_response.to_bytes().into()),
                quote_v0_discriminator: None,
                quote_l3_v0_discriminator: None,
                execute_v0_discriminator: None,
            },
        }
        .data(),
    };
    send(&mut svm, &maker, ix, &[]).unwrap();
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert_eq!(
        entry.config.response_account.to_bytes(),
        new_response.to_bytes()
    );
    let slot: QuoterSlotV0 = read_slab_slot(&svm, 0, 1);
    assert_eq!(
        slot.config.response_account.to_bytes(),
        response_account.to_bytes(),
        "the vetted copy keeps serving the old config"
    );

    // Approval must find the edited response account on the registered list,
    // so re-approving with the stale list fails, and passes once the list
    // names the new account.
    assert!(send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, true),
        &[]
    )
    .is_err());
    send(
        &mut svm,
        &maker,
        set_accounts_ix(maker.pubkey(), quoter, new_response),
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, true),
        &[],
    )
    .unwrap();
    let slot: QuoterSlotV0 = read_slab_slot(&svm, 0, 1);
    assert_eq!(
        slot.config.response_account.to_bytes(),
        new_response.to_bytes(),
        "re-approval copies the staged edit into the same slot"
    );

    // Revoking a Custom slot clears it: it has no resting state to unwind.
    // The trailing vacancy goes back — the slab shrinks to slot 0 alone and
    // the freed rent refunds the admin, far more than the transaction fee.
    let balance_before = svm.get_balance(&admin.pubkey()).unwrap();
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, false),
        &[],
    )
    .unwrap();
    assert_eq!(slab_capacity(&svm), 1);
    assert!(svm.get_balance(&admin.pubkey()).unwrap() > balance_before);
    // The staging entry survives revocation.
    let entry: QuoterV0 = read_zero_copy(&svm, &quoter);
    assert!(entry.config.is_active);
}

/// The book's approved copy lives in slot 0, and pulling its approval
/// suspends the slot instead of clearing it: the config must survive so a
/// maker can still pull orders off a killed book.
#[test]
fn revoking_the_book_suspends_its_slot() {
    let mut svm = svm();
    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 10_000_000_000).unwrap();
    set_state(&mut svm, &admin.pubkey());
    set_perp_market(&mut svm, 0);
    let user = Pubkey::new_unique();
    set_user(&mut svm, user, &admin.pubkey());

    let quoter = quoter_pda(0, &clob_id(), &user);
    let book = Pubkey::new_unique();
    send(
        &mut svm,
        &admin,
        init_quoter_ix(admin.pubkey(), quoter, user, QuoterType::Clob, book),
        &[],
    )
    .unwrap();
    create_quoter_slab(&mut svm, &admin, 0);
    send(
        &mut svm,
        &admin,
        set_accounts_ix(admin.pubkey(), quoter, book),
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, true),
        &[],
    )
    .unwrap();

    let slot: QuoterSlotV0 = read_slab_slot(&svm, 0, 0);
    assert_eq!(slot.entry.to_bytes(), quoter.to_bytes(), "a book is slot 0");
    assert!(slot.quotes());

    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, false),
        &[],
    )
    .unwrap();
    let slot: QuoterSlotV0 = read_slab_slot(&svm, 0, 0);
    assert!(!slot.is_vacant(), "the book's config survives revocation");
    assert!(slot.suspended);
    assert!(!slot.quotes());
    assert_eq!(
        slot.config.response_account.to_bytes(),
        book.to_bytes(),
        "the removal paths still find the book binding"
    );

    // Re-approval lifts the suspension in place.
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, true),
        &[],
    )
    .unwrap();
    let slot: QuoterSlotV0 = read_slab_slot(&svm, 0, 0);
    assert!(!slot.suspended && slot.quotes());
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
    let init = |authority: Pubkey| {
        init_quoter_ix(
            authority,
            quoter,
            user,
            QuoterType::Clob,
            Pubkey::new_unique(),
        )
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
    let ix = init_quoter_ix(
        admin.pubkey(),
        second,
        other_user,
        QuoterType::Clob,
        Pubkey::new_unique(),
    );
    assert!(send(&mut svm, &admin, ix, &[]).is_err());
    let market: velocity::state::perp_market::PerpMarket =
        read_zero_copy(&svm, &perp_market_pda(0));
    assert_eq!(market.clob_quoter, quoter, "the market kept its book");
}

/// Approval right-sizes the slab. A Custom approval grows the account by
/// exactly the slot it needs and the admin pays the added rent. Revoking a
/// slot in the middle leaves later occupied slots in place; revoking the last
/// occupied slot gives the trailing vacancy back and refunds the admin.
#[test]
fn approval_grows_the_slab_and_revocation_shrinks_it() {
    let mut svm = svm();
    let admin = Keypair::new();
    let maker = Keypair::new();
    for key in [&admin, &maker] {
        svm.airdrop(&key.pubkey(), 10_000_000_000).unwrap();
    }
    set_state(&mut svm, &admin.pubkey());
    set_perp_market(&mut svm, 0);
    send(&mut svm, &maker, slab_ix(maker.pubkey()), &[]).unwrap();
    assert_eq!(slab_capacity(&svm), 1);

    // Two Custom quoters, each with its own user and response account.
    let register = |svm: &mut litesvm::LiteSVM| {
        let user = Pubkey::new_unique();
        set_user(svm, user, &maker.pubkey());
        let quoter = quoter_pda(0, &clob_id(), &user);
        let response = Pubkey::new_unique();
        send(
            svm,
            &maker,
            init_quoter_ix(maker.pubkey(), quoter, user, QuoterType::Custom, response),
            &[],
        )
        .unwrap();
        send(
            svm,
            &maker,
            set_accounts_ix(maker.pubkey(), quoter, response),
            &[],
        )
        .unwrap();
        quoter
    };
    let first = register(&mut svm);
    let second = register(&mut svm);

    // Each approval grows the slab by exactly one slot; the admin pays the
    // added rent, which lands on the slab account.
    let balance_before = svm.get_balance(&admin.pubkey()).unwrap();
    let slab_lamports_before = svm.get_account(&quoter_slab_pda(0)).unwrap().lamports;
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), first, true),
        &[],
    )
    .unwrap();
    assert_eq!(slab_capacity(&svm), 2);
    assert!(svm.get_balance(&admin.pubkey()).unwrap() < balance_before);
    assert!(svm.get_account(&quoter_slab_pda(0)).unwrap().lamports > slab_lamports_before);
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), second, true),
        &[],
    )
    .unwrap();
    assert_eq!(slab_capacity(&svm), 3);
    assert_eq!(
        read_slab_slot(&svm, 0, 1).entry.to_bytes(),
        first.to_bytes()
    );
    assert_eq!(
        read_slab_slot(&svm, 0, 2).entry.to_bytes(),
        second.to_bytes()
    );

    // Revoking the middle slot cannot shrink: the slot behind it never moves.
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), first, false),
        &[],
    )
    .unwrap();
    assert_eq!(slab_capacity(&svm), 3);
    assert!(read_slab_slot(&svm, 0, 1).is_vacant());

    // Re-approval fills the vacancy instead of growing.
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), first, true),
        &[],
    )
    .unwrap();
    assert_eq!(slab_capacity(&svm), 3);
    assert_eq!(
        read_slab_slot(&svm, 0, 1).entry.to_bytes(),
        first.to_bytes()
    );
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), first, false),
        &[],
    )
    .unwrap();

    // Revoking the last occupied slot shrinks past every trailing vacancy —
    // back to slot 0 alone — and the freed rent refunds the admin, far more
    // than the transaction fee.
    let balance_before = svm.get_balance(&admin.pubkey()).unwrap();
    send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), second, false),
        &[],
    )
    .unwrap();
    assert_eq!(slab_capacity(&svm), 1);
    assert!(svm.get_balance(&admin.pubkey()).unwrap() > balance_before);
}

/// The capacity ceiling: a slab holds at most 128 slots, and only an approval
/// that needs a slot past the ceiling is refused. The full slab is synthesized
/// — approving 127 real quoters proves nothing more.
#[test]
fn a_full_slab_refuses_another_approval() {
    let mut svm = svm();
    let admin = Keypair::new();
    let maker = Keypair::new();
    for key in [&admin, &maker] {
        svm.airdrop(&key.pubkey(), 10_000_000_000).unwrap();
    }
    set_state(&mut svm, &admin.pubkey());
    set_perp_market(&mut svm, 0);
    send(&mut svm, &maker, slab_ix(maker.pubkey()), &[]).unwrap();

    // Grow the account to the ceiling and occupy every slot. Fixture-only:
    // production reaches this shape through 128 approvals.
    let slab_address = quoter_slab_pda(0);
    let mut account = svm.get_account(&slab_address).unwrap();
    account.data.resize(QuoterSlabV0::space(128), 0);
    account.data[10..12].copy_from_slice(&128u16.to_le_bytes());
    account.lamports = 100_000_000_000;
    svm.set_account(slab_address, account).unwrap();
    for index in 0..128 {
        let mut slot: QuoterSlotV0 = bytemuck::Zeroable::zeroed();
        slot.entry = Pubkey::new_unique().to_bytes().into();
        write_slab_slot(&mut svm, 0, index, &slot);
    }

    let user = Pubkey::new_unique();
    set_user(&mut svm, user, &maker.pubkey());
    let quoter = quoter_pda(0, &clob_id(), &user);
    let response = Pubkey::new_unique();
    send(
        &mut svm,
        &maker,
        init_quoter_ix(maker.pubkey(), quoter, user, QuoterType::Custom, response),
        &[],
    )
    .unwrap();
    send(
        &mut svm,
        &maker,
        set_accounts_ix(maker.pubkey(), quoter, response),
        &[],
    )
    .unwrap();
    let err = send(
        &mut svm,
        &admin,
        approve_ix(admin.pubkey(), quoter, true),
        &[],
    )
    .unwrap_err();
    assert!(
        format!("{:?}", err.err).contains("6404"),
        "expected QuoterSlabFull, got {:?}",
        err.err
    );
}
