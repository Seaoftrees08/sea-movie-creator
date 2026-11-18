#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

REQUIRE_DENY=0
SKIP_BUILD_AND_TESTS=0

for arg in "$@"; do
  case "${arg}" in
    --require-deny)
      REQUIRE_DENY=1
      ;;
    --skip-build-and-tests)
      SKIP_BUILD_AND_TESTS=1
      ;;
    -h|--help)
      echo "Usage: $0 [--require-deny] [--skip-build-and-tests]"
      exit 0
      ;;
    *)
      echo "Unknown option: ${arg}" >&2
      exit 2
      ;;
  esac
done

echo "==> Working directory: ${REPO_ROOT}"

echo "[1/6] Checking formatting (cargo fmt --all --check)..."
cargo fmt --all --check

echo "[2/6] Running Clippy (cargo clippy --workspace --all-targets --all-features -- -D warnings)..."
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "[3/6] Checking documentation warnings (cargo doc --workspace --no-deps)..."
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

if [[ "${SKIP_BUILD_AND_TESTS}" -eq 0 ]]; then
  echo "[4/6] Building all workspace targets (cargo build --workspace --all-targets)..."
  cargo build --workspace --all-targets

  echo "[5/6] Running workspace test suite (cargo test --workspace --all-targets --all-features)..."
  cargo test --workspace --all-targets --all-features
else
  echo "[4/6] Skipped build (--skip-build-and-tests)."
  echo "[5/6] Skipped tests (--skip-build-and-tests)."
fi

echo "[6/6] Checking dependencies and licenses (cargo deny check)..."
if command -v cargo-deny >/dev/null 2>&1; then
  cargo deny check
elif [[ "${REQUIRE_DENY}" -eq 1 ]]; then
  echo "Error: cargo-deny is not installed and --require-deny was specified." >&2
  echo "Install with: cargo install --locked cargo-deny" >&2
  exit 1
else
  echo "Warning: cargo-deny is not installed locally; skipping local audit (enforced on CI)."
fi

echo "All pre-PR quality gates passed."
