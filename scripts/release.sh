#!/usr/bin/env bash
# Local release build: ./scripts/release.sh [target]  → dist/sth-core-<version>-<target>.tar.gz (+ .sha256)
# Same layout as the GitHub release archives. Cross targets need the toolchain: `rustup target add <target>`
# (Linux aarch64 from x86_64 also needs gcc-aarch64-linux-gnu and CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc).
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
NAME="sth-core-${VERSION}-${TARGET}"
cargo build --release --bins --target "$TARGET"
rm -rf "dist/$NAME" && mkdir -p "dist/$NAME"
cp "target/$TARGET/release/sth-core" "target/$TARGET/release/sth-cli" README.md CHANGELOG.md "dist/$NAME/"
cp -r docs "dist/$NAME/docs"
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
(cd dist && shasum -a 256 "$NAME.tar.gz" | tee "$NAME.tar.gz.sha256")
echo "→ dist/$NAME.tar.gz"
