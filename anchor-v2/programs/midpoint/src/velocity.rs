//! Reads of velocity's global `State` account.
//!
//! The attested-flow gate asks "did the *current* flow authority co-sign this
//! transaction". "Current" is the operative word: the answer must come from
//! velocity's `State.hot_flow_authority`, which velocity's admin can rotate at
//! any time, never from a copy latched onto a quoter instance. A latched copy
//! would mean a compromised flow key stays trusted by every instance that
//! recorded it until each maker individually rotates it — a protocol-wide
//! incident with per-maker cleanup. Reading the live value costs one extra
//! account on the quote/execute legs and a 32-byte load.
//!
//! `State` is anchor-1.0 zero-copy on velocity's side, so the read is a fixed
//! byte-offset load rather than a deserialize (this program's dependency tree
//! is deliberately disjoint from velocity's — see the workspace doc). Velocity
//! already does the mirror image of this: `prop_amm.rs` reads registered CLOB
//! books at hardcoded offsets. The offset is pinned by a test that recomputes
//! it from velocity's canonical IDL, so a field inserted ahead of
//! `hot_flow_authority` fails the midpoint suite rather than silently reading
//! a neighbouring pubkey.

use {
    crate::error::MidpointError,
    anchor_lang_v2::{
        pinocchio::{account::AccountView, address::Address},
        prelude::*,
    },
};

/// Velocity's program id (`programs/velocity`, unconditional `declare_id!`).
pub const VELOCITY_PROGRAM_ID: Address =
    Address::from_str_const("vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P");

/// The one global `State` PDA: `["velocity_state"]` under velocity.
pub const VELOCITY_STATE: Address =
    Address::from_str_const("2etx5NvPNxeMZ7EfHE6GjJfW2imRYEUANehNS1WB4CVW");

/// Byte offset of `State.hot_flow_authority` in the account's data: 8 bytes of
/// anchor discriminator plus the field's 1544-byte offset within the struct.
///
/// A field added to `State` ahead of this one moves it, and this program
/// reads the live account at this offset — a stale number reads 32 bytes that
/// are not the authority. `hot_flow_authority_offset_matches_velocitys_idl`
/// walks the IDL and fails when the two disagree.
pub const STATE_HOT_FLOW_AUTHORITY_OFFSET: usize = 8 + 1544;

/// Velocity's current retail-flow attestation key, or `None` when the role is
/// unassigned (`Pubkey::default()`), which fails the gate closed rather than
/// leaving it open.
///
/// The caller is expected to have locked `state` to [`VELOCITY_STATE`] with an
/// `address =` constraint. The owner and length checks here are the second
/// layer: an address match already implies velocity is the only program that
/// could have written this data, and a never-initialized PDA has none to read.
pub fn hot_flow_authority(state: &AccountView) -> Result<Option<Address>> {
    require!(
        state.owned_by(&VELOCITY_PROGRAM_ID),
        MidpointError::InvalidVelocityState
    );
    let data = state
        .try_borrow()
        .map_err(|_| MidpointError::InvalidVelocityState)?;
    let end = STATE_HOT_FLOW_AUTHORITY_OFFSET + 32;
    require!(data.len() >= end, MidpointError::InvalidVelocityState);
    let mut key = [0u8; 32];
    key.copy_from_slice(&data[STATE_HOT_FLOW_AUTHORITY_OFFSET..end]);
    if key == [0u8; 32] {
        return Ok(None);
    }
    Ok(Some(Address::new_from_array(key)))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        serde_json::Value,
        solana_pubkey::Pubkey,
        std::{collections::HashMap, path::PathBuf},
    };

    /// `State::SIZE` minus the 8-byte discriminator, as velocity's own
    /// `const_assert` pins it.
    const VELOCITY_STATE_STRUCT_SIZE: usize = 1744;

    fn idl_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../packages/sdk/src/idl/velocity.json")
    }

    /// `(size, align)` of an IDL type under `#[repr(C)]`. u128/i128 are given
    /// align 16 (the x86-64 view velocity's `SIZE` assertion is written
    /// against); SBF's align-8 view yields the same *field offsets* here,
    /// differing only in trailing struct padding, which velocity pads out
    /// explicitly.
    fn layout(ty: &Value, types: &HashMap<String, &Value>) -> (usize, usize) {
        if let Some(name) = ty.as_str() {
            return match name {
                "u8" | "i8" | "bool" => (1, 1),
                "u16" | "i16" => (2, 2),
                "u32" | "i32" => (4, 4),
                "u64" | "i64" => (8, 8),
                "u128" | "i128" => (16, 16),
                "pubkey" | "publicKey" => (32, 1),
                other => {
                    let def = types
                        .get(other)
                        .unwrap_or_else(|| panic!("IDL type {other} not found"));
                    let kind = &def["type"];
                    match kind["kind"].as_str() {
                        // C-like enums are one byte on the wire and in memory.
                        Some("enum") => (1, 1),
                        Some("struct") => {
                            let (size, align, _) = struct_layout(&kind["fields"], types);
                            (size, align)
                        }
                        other => panic!("unsupported IDL type kind {other:?}"),
                    }
                }
            };
        }
        if let Some(array) = ty.get("array") {
            let (size, align) = layout(&array[0], types);
            let count = array[1].as_u64().expect("array length") as usize;
            return (size * count, align);
        }
        if let Some(defined) = ty.get("defined") {
            let name = defined["name"].as_str().expect("defined name");
            return layout(&Value::String(name.to_string()), types);
        }
        panic!("unsupported IDL type {ty}");
    }

    /// Lays out named fields the way `#[repr(C)]` does and returns
    /// `(size, align, offsets)`.
    fn struct_layout(
        fields: &Value,
        types: &HashMap<String, &Value>,
    ) -> (usize, usize, Vec<(String, usize)>) {
        let mut offset = 0usize;
        let mut max_align = 1usize;
        let mut offsets = Vec::new();
        for field in fields.as_array().expect("fields") {
            let (size, align) = layout(&field["type"], types);
            offset = offset.next_multiple_of(align);
            offsets.push((
                field["name"].as_str().expect("field name").to_string(),
                offset,
            ));
            offset += size;
            max_align = max_align.max(align);
        }
        (offset.next_multiple_of(max_align), max_align, offsets)
    }

    /// Velocity's `State` is read at a hardcoded offset, so the offset is
    /// pinned against the canonical IDL: inserting a field ahead of
    /// `hot_flow_authority` fails here instead of silently making every
    /// attested quote read a neighbouring pubkey.
    #[test]
    fn hot_flow_authority_offset_matches_velocitys_idl() {
        let raw = std::fs::read_to_string(idl_path()).expect("velocity IDL");
        let idl: Value = serde_json::from_str(&raw).expect("velocity IDL is json");
        let types: HashMap<String, &Value> = idl["types"]
            .as_array()
            .expect("idl types")
            .iter()
            .map(|ty| (ty["name"].as_str().expect("type name").to_string(), ty))
            .collect();
        let state = types.get("State").expect("State in the IDL");
        let (size, _, offsets) = struct_layout(&state["type"]["fields"], &types);
        assert_eq!(
            size, VELOCITY_STATE_STRUCT_SIZE,
            "State layout walk disagrees with velocity's own size assertion"
        );
        let (_, offset) = offsets
            .iter()
            .find(|(name, _)| name == "hot_flow_authority")
            .expect("State.hot_flow_authority");
        assert_eq!(8 + offset, STATE_HOT_FLOW_AUTHORITY_OFFSET);
    }

    #[test]
    fn the_state_address_is_velocitys_state_pda() {
        let program = Pubkey::new_from_array(VELOCITY_PROGRAM_ID.to_bytes());
        let (pda, _) = Pubkey::find_program_address(&[b"velocity_state"], &program);
        assert_eq!(pda.to_bytes(), VELOCITY_STATE.to_bytes());
    }
}
