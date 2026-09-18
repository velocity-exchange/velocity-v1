//! Event emission without a heap allocation.
//!
//! Anchor's `emit!` goes through `Event::data()`, which returns a `Vec<u8>` in
//! both of anchor v2's event flavours. The bytemuck flavour allocates a buffer
//! only to copy the struct into it. What reaches the runtime is
//! `[discriminator][body]` handed to `sol_log_data` as one field, so
//! [`emit_pod`] builds that on the stack and calls the syscall directly.
//!
//! The call passes one field, not two. `sol_log_data` base64 encodes each
//! slice it receives into a separate entry of a space-separated list. Decoders
//! such as velocity's `EventSubscriber` and the TypeScript SDK base64 decode
//! the whole `Program data:` line as one blob. The discriminator and the body
//! must therefore be contiguous.
//!
//! The bytes emitted are identical to `Event::data()`'s bytes. That is the
//! contract with every decoder, and the test at the bottom of this module pins
//! them against the trait impl. The CLOB program carries the same helper, plus
//! a hand-written encoder for its variable-length execute record. The two
//! programs share no crate to hold it.

use anchor_lang::prelude::*;

/// Anchor derives every event discriminator as the first 8 bytes of
/// `sha256("event:<TypeName>")`. [`emit_pod`] checks that the type it is handed
/// agrees, so this is the prefix width.
pub const DISCRIMINATOR_BYTES: usize = 8;

/// `[discriminator][body]` for a fixed-size (`#[event(bytemuck)]`) record.
/// `N` is the record's full log width; call through [`emit_pod`], which
/// derives `N` from the type and asserts at compile time that the slice
/// math below is exact. A wrong `N` truncates or pads the event.
pub fn pod_log_bytes<E, const N: usize>(record: &E) -> [u8; N]
where
    E: Discriminator + bytemuck::Pod,
{
    let mut bytes = [0u8; N];
    bytes[..DISCRIMINATOR_BYTES].copy_from_slice(E::DISCRIMINATOR);
    bytes[DISCRIMINATOR_BYTES..].copy_from_slice(bytemuck::bytes_of(record));
    bytes
}

/// Emit a fixed-size (`#[event(bytemuck)]`) record. The bytes match what
/// `emit!` logs, but this macro builds them on the stack. It takes the record's
/// struct literal, so call sites read the way `emit!` did.
macro_rules! emit_pod {
    ($ty:ident { $($field:tt)* }) => {{
        const LOG_BYTES: usize =
            $crate::emit::DISCRIMINATOR_BYTES + ::core::mem::size_of::<$ty>();
        const _: () = ::core::assert!(
            <$ty as anchor_lang::Discriminator>::DISCRIMINATOR.len()
                == $crate::emit::DISCRIMINATOR_BYTES,
            "event discriminator is not 8 bytes wide",
        );

        anchor_lang::sol_log_data(&[&$crate::emit::pod_log_bytes::<$ty, LOG_BYTES>(&$ty {
            $($field)*
        })]);
    }};
}

pub(crate) use emit_pod;

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::events::{MidpointExecuteRecordV0, MIDPOINT_EVENT_VERSION},
    };

    /// The stack-built bytes must equal anchor's. A decoder keys on the
    /// discriminator and then reads the body as borsh.
    #[test]
    fn the_execute_record_emits_the_bytes_the_event_impl_would() {
        const LOG_BYTES: usize =
            DISCRIMINATOR_BYTES + core::mem::size_of::<MidpointExecuteRecordV0>();
        let record = MidpointExecuteRecordV0 {
            user_authority: Address::new_from_array([0xAB; 32]),
            ts: -7,
            slot: 9,
            mid_price: 100_000_000,
            base_size: 11,
            quote_size: 13,
            configured_market_index: 17,
            sub_account_id: 19,
            direction: 1,
            version: MIDPOINT_EVENT_VERSION,
            _pad: [0; 2],
        };

        assert_eq!(
            pod_log_bytes::<MidpointExecuteRecordV0, LOG_BYTES>(&record).as_slice(),
            Event::data(&record).as_slice()
        );
    }
}
