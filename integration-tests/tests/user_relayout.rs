//! `extend_account` on a `User` in the layout before `open_orders` became a
//! `u16`. The deployed program rewrites the account into the current layout.

use {
    anchor_lang::{Discriminator, InstructionData, ToAccountMetas},
    bytemuck::Zeroable,
    solana_account::Account,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    velocity::{
        error::ErrorCode,
        state::{
            traits::Size,
            user::{LegacyUser, PositionFlag, User, LEGACY_USER_LEN},
        },
    },
    velocity_integration_tests::*,
};

fn legacy_user(authority: &Pubkey) -> Box<LegacyUser> {
    let mut legacy: Box<LegacyUser> = Box::new(Zeroable::zeroed());
    legacy.authority = anchor_lang::prelude::Pubkey::new_from_array(authority.to_bytes());
    for (i, position) in legacy.perp_positions.iter_mut().enumerate() {
        position.base_asset_amount = 1_000 * (i as i64 + 1);
        position.quote_asset_amount = -2_000 * (i as i64 + 1);
        position.market_index = i as u16;
        position.open_orders = 3 + i as u8;
        position.position_flag = PositionFlag::IsolatedPosition as u8;
    }

    legacy.orders[0].order_id = 7;
    legacy.sub_account_id = 4;
    legacy.status = 1;
    legacy.open_orders = 9;
    legacy.has_open_order = true;
    legacy.pool_id = 2;
    legacy.equity_floor = 5_000_000;
    legacy.equity_floor_buffer = 1_000_000;
    legacy
}

fn set_raw_user(svm: &mut litesvm::LiteSVM, address: Pubkey, body: &[u8], len: usize) {
    let mut data = vec![0u8; len];
    data[..8].copy_from_slice(User::DISCRIMINATOR);
    data[8..8 + body.len()].copy_from_slice(body);
    let account = Account {
        lamports: svm.minimum_balance_for_rent_exemption(len),
        data,
        owner: velocity_id(),
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(address, account).unwrap();
}

fn extend_account_ix(admin: &Pubkey, account: Pubkey) -> Instruction {
    Instruction {
        program_id: velocity_id(),
        accounts: velocity::accounts::ExtendAccount {
            state: state_pda(),
            payer: *admin,
            authority: *admin,
            account,
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
        data: velocity::instruction::ExtendAccount {}.data(),
    }
}

#[test]
fn extend_account_rewrites_a_legacy_user_into_the_current_layout() {
    let mut svm = svm();
    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();
    set_state(&mut svm, &admin.pubkey());

    let address = Pubkey::new_unique();
    let legacy = legacy_user(&admin.pubkey());
    set_raw_user(
        &mut svm,
        address,
        bytemuck::bytes_of(&*legacy),
        LEGACY_USER_LEN,
    );

    send(
        &mut svm,
        &admin,
        extend_account_ix(&admin.pubkey(), address),
        &[],
    )
    .unwrap();

    let account = svm.get_account(&address).unwrap();
    assert_eq!(account.data.len(), User::SIZE);
    assert!(account.lamports >= svm.minimum_balance_for_rent_exemption(User::SIZE));

    // The host runs the same conversion. The onchain bytes must match it exactly.
    let mut expected = account.data.clone();
    expected[8..].fill(0xFF);
    legacy.write(&mut expected);
    assert_eq!(account.data, expected);

    let user: User = read_zero_copy(&svm, &address);
    for (i, position) in user.perp_positions.iter().enumerate() {
        assert_eq!(position.base_asset_amount, 1_000 * (i as i64 + 1));
        assert_eq!(position.quote_asset_amount, -2_000 * (i as i64 + 1));
        assert_eq!(position.market_index, i as u16);
        assert_eq!(position.open_orders, 3 + i as u16);
        assert_eq!(position.position_flag, PositionFlag::IsolatedPosition as u8);
        assert_eq!(position.reduce_only_clob_orders, 0);
    }

    assert_eq!(user.orders[0].order_id, 7);
    assert_eq!(user.sub_account_id, 4);
    assert_eq!(user.status, 1);
    assert_eq!(user.open_orders, 9);
    assert!(user.has_open_order);
    assert_eq!(user.pool_id, 2);
    assert_eq!(user.equity_floor, 5_000_000);
    assert_eq!(user.equity_floor_buffer, 1_000_000);

    // A second crank finds the account at size and leaves it alone.
    send(
        &mut svm,
        &admin,
        extend_account_ix(&admin.pubkey(), address),
        &[],
    )
    .unwrap();
    assert_eq!(svm.get_account(&address).unwrap().data, expected);
}

#[test]
fn extend_account_refuses_a_user_at_any_other_short_length() {
    let mut svm = svm();
    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();
    set_state(&mut svm, &admin.pubkey());

    let address = Pubkey::new_unique();
    set_raw_user(&mut svm, address, &[], LEGACY_USER_LEN - 16);

    let err = send(
        &mut svm,
        &admin,
        extend_account_ix(&admin.pubkey(), address),
        &[],
    )
    .unwrap_err();
    assert_velocity_error(&err, ErrorCode::InvalidAccountExtension);
    assert_eq!(
        svm.get_account(&address).unwrap().data.len(),
        LEGACY_USER_LEN - 16
    );
}
