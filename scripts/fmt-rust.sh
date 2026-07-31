#!/usr/bin/env bash
# Formats every Rust codebase in the repo with nightly rustfmt. rustfmt.toml
# uses nightly-only import-merging options that stable `cargo fmt` ignores
# (leaving new imports unmerged, which fails CI); always format through this
# script (`bun run fmt:rust`) or with an explicit `cargo +nightly fmt`.
#
# Pass --check to verify without writing (what CI runs, spread across jobs).
# Override the toolchain with RUSTFMT_TOOLCHAIN (defaults to `nightly`).
set -euo pipefail
cd "$(dirname "$0")/.."

tc="${RUSTFMT_TOOLCHAIN:-nightly}"
check="${1:-}"

# Program workspace (programs/*, crates/*), then the rust/, anchor-v2/, and
# integration-tests/ workspaces.
cargo "+$tc" fmt --all -- $check
cargo "+$tc" fmt --manifest-path rust/Cargo.toml --all -- $check
cargo "+$tc" fmt --manifest-path anchor-v2/Cargo.toml --all -- $check
cargo "+$tc" fmt --manifest-path integration-tests/Cargo.toml --all -- $check

# Each fuzz/<crate>/ is its own standalone workspace.
for m in fuzz/*/Cargo.toml; do
	cargo "+$tc" fmt --manifest-path "$m" --all -- $check
done

# The velocity-rs example crates (all but dlob-builder) are not workspace
# members and don't resolve under cargo; format with raw rustfmt at each
# crate's own edition.
for d in rust/velocity-rs/examples/*/; do
	ed=$(sed -nE 's/^edition = "([0-9]+)"$/\1/p' "$d/Cargo.toml" | head -1)
	files=$(git ls-files "${d}**/*.rs" "${d}*.rs")
	if [ -n "$files" ]; then
		echo "$files" | xargs rustup run "$tc" rustfmt --edition "${ed:-2021}" $check
	fi
done

echo "rust fmt ($tc${check:+, $check}) OK"
