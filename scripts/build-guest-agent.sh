#!/usr/bin/env bash
# Builds the static vat-guest agent (aarch64-unknown-linux-musl) that vat
# embeds for `vat machine`, and records the hash of the sources it was built
# from so a stale binary fails `cargo test`.
#
# Needs rustup with the musl target:
#   rustup target add aarch64-unknown-linux-musl
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
crate="$root/guest-agent"
target=aarch64-unknown-linux-musl

# Use rustup's toolchain binaries explicitly: a Homebrew cargo/rustc earlier
# in PATH has no cross targets.
RUSTC=$(rustup +stable which rustc) \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
  "$(rustup +stable which cargo)" build --release --locked \
  --manifest-path "$crate/Cargo.toml" --target "$target"

mkdir -p "$crate/dist"
cp "$crate/target/$target/release/vat-guest" "$crate/dist/vat-guest-aarch64"
cat "$crate/Cargo.toml" "$crate/Cargo.lock" "$crate/src/main.rs" \
  | shasum -a 256 | cut -d' ' -f1 > "$crate/dist/SOURCE_SHA256"
ls -l "$crate/dist/vat-guest-aarch64"
