//! The litesvm harness the CLOB and midpoint integration suites share.
//!
//! Both suites drive a real `.so` over litesvm and read a quoter response out
//! of the account the return-data pointer names. That is the quoter interface,
//! so the harness for it belongs to neither program. Each suite keeps its own
//! `Ctx`, because the keypairs a market needs are not the keypairs a spline
//! quoter needs.

// Every send helper returns litesvm's `FailedTransactionMetadata` by value;
// boxing a test harness's error type buys nothing.
#![allow(clippy::result_large_err)]

use {
    anchor_lang::{prelude::Address, solana_program::instruction::Instruction},
    anchor_v2_testing::{
        Keypair, LiteSVM, Message, Signer, VersionedMessage, VersionedTransaction,
    },
    litesvm::types::{FailedTransactionMetadata, TransactionMetadata},
    solana_pubkey::Pubkey,
};

/// The two crates spell one key two ways. Tests hold `Pubkey`, and the wire
/// takes `Address`.
pub fn addr(pk: Pubkey) -> Address {
    Address::new_from_array(pk.to_bytes())
}

pub fn system_program() -> Pubkey {
    "11111111111111111111111111111111".parse().unwrap()
}

pub fn parse_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

/// Where a quoter wrote its response, and the bytes it wrote. The offset is
/// returned rather than dropped, because a quoter whose region sits at a fixed
/// account offset asserts on it.
pub struct Response {
    pub offset: usize,
    pub bytes: Vec<u8>,
}

/// Send `ix` paid for by `payer`, signed by whichever of `candidates` the
/// instruction names as a signer.
///
/// `compute_unit_limit` prefixes a ComputeBudget instruction. A full-side
/// quote or a deep execute runs past the 200k default, and raising it in the
/// transaction is what a real caller does.
pub fn send(
    svm: &mut LiteSVM,
    payer: &Keypair,
    candidates: &[&Keypair],
    ix: Instruction,
    compute_unit_limit: Option<u32>,
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    // Fresh blockhash per send so identical instruction streams do not dedupe.
    svm.expire_blockhash();
    let blockhash = svm.latest_blockhash();
    let ixs: Vec<Instruction> = compute_unit_limit
        .map(compute_unit_limit_ix)
        .into_iter()
        .chain(core::iter::once(ix.clone()))
        .collect();

    let msg = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &blockhash);
    let mut signers: Vec<&dyn Signer> = vec![payer];
    for candidate in candidates {
        let needed = ix
            .accounts
            .iter()
            .any(|meta| meta.is_signer && meta.pubkey.to_bytes() == candidate.pubkey().to_bytes());
        let already = signers
            .iter()
            .any(|signer| signer.pubkey().to_bytes() == candidate.pubkey().to_bytes());
        if needed && !already {
            signers.push(*candidate);
        }
    }

    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    svm.send_transaction(tx)
}

/// The response bytes the pointer in `meta`'s return data designates. The
/// pointer names an offset into `account`, because return data is capped at
/// 1024 bytes and a full ladder is wider.
pub fn read_response(
    svm: &LiteSVM,
    program_id: Pubkey,
    account: Pubkey,
    meta: &TransactionMetadata,
) -> Response {
    assert_eq!(
        meta.return_data.program_id.to_bytes(),
        program_id.to_bytes()
    );

    let offset = parse_u32(&meta.return_data.data) as usize;
    let len = parse_u32(&meta.return_data.data[4..]) as usize;
    let data = svm.get_account(&account).unwrap().data;
    Response {
        offset,
        bytes: data[offset..offset + len].to_vec(),
    }
}

fn compute_unit_limit_ix(limit: u32) -> Instruction {
    Instruction {
        program_id: "ComputeBudget111111111111111111111111111111"
            .parse()
            .unwrap(),
        accounts: Vec::new(),
        // Tag 2 is SetComputeUnitLimit(u32).
        data: [&[2u8][..], &limit.to_le_bytes()[..]].concat(),
    }
}
