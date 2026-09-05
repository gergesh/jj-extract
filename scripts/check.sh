#!/usr/bin/env bash
# Run every local quality gate used by jj-extract contributors.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo build --locked
tests/integration.sh
python3 tests/parallel_formatting.py
