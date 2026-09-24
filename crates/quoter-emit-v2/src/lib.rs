//! Event emission without a heap allocation, for the anchor v2 quoters.
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
//! contract with every decoder. [`assert_pod_matches_event`] is how a program
//! pins one of its records against the trait implementation.
//!
//! A variable-length record needs a streaming encoder rather than this copy.
//! The CLOB owns the one such record and keeps its writer.

// The programs reach anchor through this crate, so a macro expansion does not
// depend on what the caller named its own dependency.
pub use {anchor_lang, bytemuck};

/// Anchor derives every event discriminator as the first 8 bytes of
/// `sha256("event:<TypeName>")`. [`emit_pod`] checks that the type it is handed
/// agrees, so this constant is the prefix width.
pub const DISCRIMINATOR_BYTES: usize = 8;

/// `[discriminator][body]` for a fixed-size `#[event(bytemuck)]` record.
/// `N` is the record's full log width; call through [`emit_pod`], which
/// derives `N` from the type and checks the discriminator width at compile
/// time. A wrong `N` would truncate or pad the event.
pub fn pod_log_bytes<E, const N: usize>(record: &E) -> [u8; N]
where
    E: anchor_lang::Discriminator + bytemuck::Pod,
{
    let mut bytes = [0u8; N];
    bytes[..DISCRIMINATOR_BYTES].copy_from_slice(E::DISCRIMINATOR);
    bytes[DISCRIMINATOR_BYTES..].copy_from_slice(bytemuck::bytes_of(record));
    bytes
}

/// Emit a fixed-size `#[event(bytemuck)]` record. It logs the same bytes as
/// `emit!`, built on the stack. It takes the record's struct literal, so a call
/// site reads the way an `emit!` call site reads.
#[macro_export]
macro_rules! emit_pod {
    ($ty:ident { $($field:tt)* }) => {{
        const LOG_BYTES: usize =
            $crate::DISCRIMINATOR_BYTES + ::core::mem::size_of::<$ty>();
        const _: () = ::core::assert!(
            <$ty as $crate::anchor_lang::Discriminator>::DISCRIMINATOR.len()
                == $crate::DISCRIMINATOR_BYTES,
            "event discriminator is not 8 bytes wide",
        );

        $crate::anchor_lang::sol_log_data(&[&$crate::pod_log_bytes::<$ty, LOG_BYTES>(&$ty {
            $($field)*
        })]);
    }};
}

/// Assert that [`pod_log_bytes`] at the width [`emit_pod`] computes is what
/// `Event::data()` returns for the same record.
#[macro_export]
macro_rules! assert_pod_matches_event {
    ($ty:ident { $($field:tt)* }) => {{
        const LOG_BYTES: usize =
            $crate::DISCRIMINATOR_BYTES + ::core::mem::size_of::<$ty>();
        let record = $ty { $($field)* };
        ::core::assert_eq!(
            $crate::pod_log_bytes::<$ty, LOG_BYTES>(&record).as_slice(),
            <$ty as $crate::anchor_lang::Event>::data(&record).as_slice(),
            ::core::concat!(
                ::core::stringify!($ty),
                " emit path diverged from Event::data()",
            ),
        );
    }};
}
