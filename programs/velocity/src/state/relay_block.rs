//! [`relay_spec::RelayBlockV0`] wrapped for anchor hosting.
//!
//! The spec crate is deliberately framework-free (bytemuck-only), so it
//! cannot implement anchor's `IdlBuild` — and without it, `anchor idl
//! build` rejects any account struct carrying the block. This wrapper owns
//! that coupling: it derefs to the spec type (hosts call `init`,
//! `write_resolvers`, and the `ConditionBlock` surface straight through)
//! and describes itself to the IDL as what it is on the wire — an opaque
//! byte region of the instantiation's exact size.

use relay_spec::RelayBlockV0;

/// One field hosting everything relay needs: the spec header, the
/// condition slots, and the resolver account list region. See
/// [`relay_spec::RelayBlockV0`].
#[derive(Clone, Copy, Debug, Default)]
#[repr(transparent)]
pub struct RelayBlock<const CONDITIONS: usize, const RESOLVER_CAPACITY: usize>(
    pub RelayBlockV0<CONDITIONS, RESOLVER_CAPACITY>,
);

impl<const C: usize, const R: usize> core::ops::Deref for RelayBlock<C, R> {
    type Target = RelayBlockV0<C, R>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const C: usize, const R: usize> core::ops::DerefMut for RelayBlock<C, R> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

// repr(transparent) over a Pod type.
unsafe impl<const C: usize, const R: usize> relay_spec::bytemuck::Zeroable for RelayBlock<C, R> {}
unsafe impl<const C: usize, const R: usize> relay_spec::bytemuck::Pod for RelayBlock<C, R> {}

/// The condition surface, by delegation, so hosts can name the wrapper in
/// trait calls without deref gymnastics.
impl<const C: usize, const R: usize> relay_spec::ConditionBlock for RelayBlock<C, R> {
    const NUM_CONDITIONS: usize = C;

    fn block(&self) -> &[u8] {
        relay_spec::ConditionBlock::block(&self.0)
    }

    fn block_mut(&mut self) -> &mut [u8] {
        relay_spec::ConditionBlock::block_mut(&mut self.0)
    }
}

#[cfg(feature = "idl-build")]
impl<const C: usize, const R: usize> anchor_lang::idl::IdlBuild for RelayBlock<C, R> {
    fn create_type() -> Option<anchor_lang::idl::types::IdlTypeDef> {
        use anchor_lang::idl::types::*;
        Some(IdlTypeDef {
            name: Self::get_full_path(),
            docs: vec![format!(
                "relay_spec::RelayBlockV0<{C}, {R}>: spec header, {C} condition slots, \
                 and a {R}-slot resolver account list, as one opaque wire region"
            )],
            serialization: IdlSerialization::BytemuckUnsafe,
            repr: Some(IdlRepr::C(IdlReprModifier {
                packed: false,
                align: None,
            })),
            generics: vec![],
            ty: IdlTypeDefTy::Struct {
                fields: Some(IdlDefinedFields::Named(vec![IdlField {
                    name: "bytes".into(),
                    docs: vec![],
                    ty: IdlType::Array(
                        Box::new(IdlType::U8),
                        IdlArrayLen::Value(RelayBlockV0::<C, R>::SIZE),
                    ),
                }])),
            },
        })
    }

    fn insert_types(
        types: &mut std::collections::BTreeMap<String, anchor_lang::idl::types::IdlTypeDef>,
    ) {
        if let Some(ty) = Self::create_type() {
            types.insert(ty.name.clone(), ty);
        }
    }

    fn get_full_path() -> String {
        format!("RelayBlock{C}x{R}")
    }
}
