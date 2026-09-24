//! The market's CLOB book, installed the way production installs it.
//!
//! Every perp fill routes through one router pass, and the market's book is a
//! mandatory source in it. A fixture without a book can place no perp order, so
//! the harness loads the CLOB program, creates a book whose place authority is
//! the market's quoter slab, and registers and approves it as the market's Clob
//! quoter through the real instructions.
//!
//! The account lists come from the program's own `Accounts` structs, so an
//! account the program adds or renames breaks this file at compile time.

use {
    anchor_lang::{InstructionData, ToAccountMetas},
    crucible_test_context::{AccountBuilderBase, TestContext},
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    velocity::{
        instructions::{
            InitializeQuoterArgs, InitializeQuoterSlabArgs, QuoterAccountMetaArg,
            UpdateQuoterAccountsArgs, UpdateQuoterApprovedArgs,
        },
        state::prop_amm::QuoterType,
    },
};

/// The perp market the book serves. The fixture has one perp market.
pub const MARKET_INDEX: u16 = 0;

/// Where the CLOB program is looked for, relative to the harness run directory.
///
/// A fuzz bundle stages it next to `velocity.so`. A local checkout builds it
/// under `anchor-v2/`.
const CLOB_SO_PATHS: [&str; 2] = [
    "../../target/deploy/clob.so",
    "../../anchor-v2/target/deploy/clob.so",
];

/// Orders the book can hold. The arena is sized from the account length.
const BOOK_CAPACITY: usize = 256;

fn velocity_program_id() -> Pubkey {
    Pubkey::new_from_array(velocity::ID.to_bytes())
}

fn system_program_id() -> Pubkey {
    Pubkey::new_from_array([0u8; 32])
}

pub fn clob_program_id() -> Pubkey {
    Pubkey::from_str_const("BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU")
}

fn bpf_loader_upgradeable_id() -> Pubkey {
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111")
}

fn rent_sysvar_id() -> Pubkey {
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111")
}

/// Anchor's default instruction discriminator, `sha256("global:<name>")[..8]`.
fn discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("global:{name}").as_bytes())[..8]
        .try_into()
        .unwrap()
}

pub fn quoter_slab_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[b"quoter_slab", &MARKET_INDEX.to_le_bytes()],
        &velocity_program_id(),
    )
    .0
}

fn quoter_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"quoter",
            &MARKET_INDEX.to_le_bytes(),
            clob_program_id().as_ref(),
            user.as_ref(),
        ],
        &velocity_program_id(),
    )
    .0
}

fn program_data_pda(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program.as_ref()], &bpf_loader_upgradeable_id()).0
}

/// The accounts every CLOB-routed instruction carries.
#[derive(Clone, Copy)]
pub struct ClobAccounts {
    pub quoter_slab: Pubkey,
    pub book: Pubkey,
    pub program: Pubkey,
}

impl ClobAccounts {
    /// The quoter section a router fill appends: the market's book entry and
    /// the accounts its CPI resolves against.
    pub fn route_metas(&self) -> [AccountMeta; 3] {
        [
            AccountMeta::new_readonly(self.quoter_slab, false),
            AccountMeta::new(self.book, false),
            AccountMeta::new_readonly(self.program, false),
        ]
    }
}

/// The book's placement rules, as borsh `MarketConfigV0`.
///
/// The step and the minimum match the perp market's, so an order the market
/// accepts can always rest.
fn book_config(order_step_size: u64, min_order_size: u64) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&MARKET_INDEX.to_le_bytes());
    v.extend_from_slice(&1_000_000_000u64.to_le_bytes()); // base_precision
    v.extend_from_slice(&1u64.to_le_bytes()); // order_tick_size
    v.extend_from_slice(&order_step_size.to_le_bytes());
    v.extend_from_slice(&min_order_size.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes()); // blocking_min_size
    v.extend_from_slice(&0u32.to_le_bytes()); // default_activation_delay
    v.extend_from_slice(&20u32.to_le_bytes()); // max_activation_delay
    v.extend_from_slice(&2u32.to_le_bytes()); // unknown_user_grace_slots
    v.extend_from_slice(&1u32.to_le_bytes()); // evict_threshold_per_side
    v.extend_from_slice(&128u16.to_le_bytes()); // max_quote_levels
    v.extend_from_slice(&64u16.to_le_bytes()); // max_execute_fills
    v.extend_from_slice(&32u16.to_le_bytes()); // max_execute_users
    v
}

fn send(ctx: &mut TestContext, ix: Instruction, signers: &[&Keypair], what: &str) {
    let outcome = ctx
        .raw_call(ix)
        .signers(signers)
        .send()
        .unwrap_or_else(|err| panic!("{what}: {err:#}"));
    assert!(
        outcome.is_success(),
        "{what} failed:\n  {}",
        outcome.logs().join("\n  ")
    );
}

/// Load the CLOB program, and give it the program-data account that quoter
/// approval reads its deploy slot from. LiteSVM loads a program without one.
fn load_clob_program(ctx: &mut TestContext) {
    let path = CLOB_SO_PATHS
        .iter()
        .find(|path| std::path::Path::new(path).exists())
        .expect("clob.so missing: run `bun run program:build:clob`");
    ctx.add_program(&clob_program_id(), path)
        .expect("add clob.so");

    // A `ProgramData` account: tag 3, deploy slot 0, no upgrade authority.
    let mut data = vec![0u8; 45];
    data[0] = 3;
    ctx.create_account()
        .pubkey(program_data_pda(&clob_program_id()))
        .owner(bpf_loader_upgradeable_id())
        .lamports(1_000_000_000)
        .data(&data)
        .create()
        .expect("clob program data");
}

/// Create the book. It signs its own creation, and its place authority is the
/// market's quoter slab, so every placement has to come through velocity.
fn create_book(
    ctx: &mut TestContext,
    admin: &Keypair,
    order_step_size: u64,
    min_order_size: u64,
) -> Pubkey {
    let book = Keypair::new();
    ctx.create_account()
        .pubkey(book.pubkey())
        .owner(clob_program_id())
        .lamports(10_000_000_000)
        .data(&vec![0u8; 32 * 1024 + BOOK_CAPACITY * 128])
        .create()
        .expect("book account");

    let mut data = discriminator("initialize_market_v0").to_vec();
    // The book refuses a zero minimum. A market with no minimum of its own
    // bounds nothing, so one step serves.
    let book_min_order_size = if min_order_size == 0 {
        order_step_size
    } else {
        min_order_size
    };
    data.extend_from_slice(&book_config(order_step_size, book_min_order_size));
    let ix = Instruction {
        program_id: clob_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(quoter_slab_pda(), false),
            AccountMeta::new_readonly(quoter_slab_pda(), false),
            AccountMeta::new(book.pubkey(), true),
        ],
        data,
    };

    send(ctx, ix, &[admin, &book], "initialize_market_v0");
    book.pubkey()
}

fn initialize_slab_ix(admin: &Keypair, perp_market: Pubkey) -> Instruction {
    Instruction {
        program_id: velocity_program_id(),
        accounts: velocity::accounts::InitializeQuoterSlab {
            payer: admin.pubkey(),
            perp_market,
            quoter_slab: quoter_slab_pda(),
            rent: rent_sysvar_id(),
            system_program: system_program_id(),
        }
        .to_account_metas(None),
        data: velocity::instruction::InitializeQuoterSlab {
            args: InitializeQuoterSlabArgs {
                market_index: MARKET_INDEX,
            },
        }
        .data(),
    }
}

/// The staging entry that names the book as the market's Clob quoter.
struct QuoterEntry {
    quoter: Pubkey,
    state: Pubkey,
    perp_market: Pubkey,
    book: Pubkey,
}

impl QuoterEntry {
    fn initialize_ix(&self, admin: &Keypair, quoter_user: Pubkey) -> Instruction {
        Instruction {
            program_id: velocity_program_id(),
            accounts: velocity::accounts::InitializeQuoter {
                state: self.state,
                payer: admin.pubkey(),
                authority: admin.pubkey(),
                quoter: self.quoter,
                perp_market: self.perp_market,
                quoter_slab: Some(quoter_slab_pda()),
                quoter_program: clob_program_id(),
                user: quoter_user,
                rent: rent_sysvar_id(),
                system_program: system_program_id(),
            }
            .to_account_metas(None),
            data: velocity::instruction::InitializeQuoter {
                args: InitializeQuoterArgs {
                    market_index: MARKET_INDEX,
                    quoter_type: QuoterType::Clob,
                    response_account: self.book,
                    quote_v0_discriminator: discriminator("quote_v0"),
                    quote_l3_v0_discriminator: discriminator("quote_l3_v0"),
                    execute_v0_discriminator: discriminator("execute_v0"),
                },
            }
            .data(),
        }
    }

    /// The quote leg forwards the book alone. Execute adds the slab, which
    /// signs the CPI.
    fn accounts_ix(&self, admin: &Keypair) -> Instruction {
        Instruction {
            program_id: velocity_program_id(),
            accounts: velocity::accounts::UpdateQuoterAccounts {
                authority: admin.pubkey(),
                quoter: self.quoter,
                state: Some(self.state),
            }
            .to_account_metas(None),
            data: velocity::instruction::UpdateQuoterAccounts {
                args: UpdateQuoterAccountsArgs {
                    metas: vec![
                        QuoterAccountMetaArg {
                            pubkey: self.book,
                            is_writable: true,
                        },
                        QuoterAccountMetaArg {
                            pubkey: quoter_slab_pda(),
                            is_writable: false,
                        },
                    ],
                    quote_indexes: vec![0],
                    execute_indexes: vec![0, 1],
                },
            }
            .data(),
        }
    }

    /// Approval copies the staged entry into the slab, which is the copy fills
    /// read.
    fn approve_ix(&self, admin: &Keypair) -> Instruction {
        Instruction {
            program_id: velocity_program_id(),
            accounts: velocity::accounts::UpdateQuoterApproved {
                admin: admin.pubkey(),
                state: self.state,
                quoter: self.quoter,
                perp_market: self.perp_market,
                quoter_slab: quoter_slab_pda(),
                quoter_program: clob_program_id(),
                quoter_program_data: Some(program_data_pda(&clob_program_id())),
                clob_market: Some(self.book),
                system_program: system_program_id(),
            }
            .to_account_metas(None),
            data: velocity::instruction::UpdateQuoterApproved {
                args: UpdateQuoterApprovedArgs { approved: true },
            }
            .data(),
        }
    }
}

/// Register the book as the market's Clob quoter and approve it.
fn register_book(ctx: &mut TestContext, admin: &Keypair, entry: &QuoterEntry, quoter_user: Pubkey) {
    send(
        ctx,
        initialize_slab_ix(admin, entry.perp_market),
        &[admin],
        "initialize_quoter_slab",
    );
    send(
        ctx,
        entry.initialize_ix(admin, quoter_user),
        &[admin],
        "initialize_quoter",
    );
    send(
        ctx,
        entry.accounts_ix(admin),
        &[admin],
        "update_quoter_accounts",
    );
    send(
        ctx,
        entry.approve_ix(admin),
        &[admin],
        "update_quoter_approved",
    );
}

/// Install the market's book and return the accounts that route to it.
///
/// `perp_market` must already name [`quoter_slab_pda`] as its slab, which is
/// what `initialize_perp_market` writes.
pub fn install(
    ctx: &mut TestContext,
    admin: &Keypair,
    state: Pubkey,
    perp_market: Pubkey,
    order_step_size: u64,
    min_order_size: u64,
    quoter_user: Pubkey,
) -> ClobAccounts {
    load_clob_program(ctx);
    let book = create_book(ctx, admin, order_step_size, min_order_size);
    let entry = QuoterEntry {
        quoter: quoter_pda(&quoter_user),
        state,
        perp_market,
        book,
    };
    register_book(ctx, admin, &entry, quoter_user);
    ClobAccounts {
        quoter_slab: quoter_slab_pda(),
        book,
        program: clob_program_id(),
    }
}
