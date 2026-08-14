#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

pnpm audit --audit-level high
cargo audit --file Cargo.lock --deny warnings
cargo test -p hearth-core -p hearth-tools -p hearth-daemon -p hearth-cli --all-targets
cargo clippy -p hearth-core -p hearth-tools -p hearth-daemon -p hearth-cli --all-targets -- -D warnings
