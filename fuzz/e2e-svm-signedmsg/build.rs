//! Derive a crucible-consumable IDL from the canonical velocity IDL.
//!
//! Anchor's IDL builder monomorphizes a const-generic type such as
//! `RelayBlock<2, 8>` to the concrete name `RelayBlock2x8` (a zero-generic
//! struct of opaque bytes), but it still emits `generics: [2, 8]` on every
//! field that references the type. The TypeScript SDK ignores those redundant
//! generics; `crucible-idl-gen` honors them and emits `RelayBlock2x8<2, 8>`,
//! which does not match the zero-generic definition and fails to compile.
//!
//! Strip the redundant generics off `defined` references whose generic
//! arguments are all `const` (the monomorphized case), so crucible sees the
//! reference as the concrete type its definition already is. The canonical IDL
//! is the single source; this only reshapes it for crucible, in the build tree.

use {
    serde_json::Value,
    std::{env, fs, path::PathBuf},
};

fn strip_redundant_const_generics(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Object(defined)) = map.get_mut("defined") {
                let all_const = defined
                    .get("generics")
                    .and_then(|g| g.as_array())
                    .map(|generics| {
                        !generics.is_empty()
                            && generics
                                .iter()
                                .all(|g| g.get("kind").and_then(|k| k.as_str()) == Some("const"))
                    })
                    .unwrap_or(false);
                if all_const {
                    defined.remove("generics");
                }
            }

            for child in map.values_mut() {
                strip_redundant_const_generics(child);
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                strip_redundant_const_generics(child);
            }
        }
        _ => {}
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let canonical = manifest.join("../../packages/sdk/src/idl/velocity.json");
    println!("cargo:rerun-if-changed={}", canonical.display());

    let raw = fs::read_to_string(&canonical).expect("read canonical velocity IDL");
    let mut idl: Value = serde_json::from_str(&raw).expect("parse velocity IDL");
    strip_redundant_const_generics(&mut idl);

    let out = manifest.join("velocity.fuzz-idl.json");
    fs::write(&out, serde_json::to_string(&idl).expect("serialize IDL")).expect("write fuzz IDL");
}
