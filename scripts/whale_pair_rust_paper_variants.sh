#!/usr/bin/env bash
# Backwards-compatible wrapper for the canonical whale-pair Rust paper variant launcher.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$SCRIPT_DIR/../execution/whale_pair/cmd/whale_pair_rust_paper_variants.sh" "$@"
