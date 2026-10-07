#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# jlsp Cross-Platform Packaging Script (macOS / Linux / Windows)
# ==============================================================================
# Usage:
#   ./scripts/package.sh [options] [targets...]
#
# Options:
#   --use-cross        Use `cross` (Docker container) instead of cargo for cross-compilation
#   --release          Build in release mode (default: true)
#   --all              Build all standard release targets
#   --out-dir <dir>    Output directory for archives (default: ./dist)
#
# Examples:
#   ./scripts/package.sh                           # Builds native target only
#   ./scripts/package.sh --all --use-cross         # Builds all targets using cross
#   ./scripts/package.sh x86_64-apple-darwin aarch64-apple-darwin
# ==============================================================================

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${PROJECT_ROOT}"

APP_NAME="jlsp"
VERSION=$(grep -m1 '^version = ' Cargo.toml | cut -d '"' -f2 || echo "0.1.0")
OUT_DIR="${PROJECT_ROOT}/dist"
USE_CROSS=false
BUILD_ALL=false
SELECTED_TARGETS=()

# Available default tier-1 targets
ALL_TARGETS=(
  "x86_64-apple-darwin"
  "aarch64-apple-darwin"
  "x86_64-unknown-linux-musl"
  "aarch64-unknown-linux-musl"
  "x86_64-unknown-linux-gnu"
  "x86_64-pc-windows-gnu"
)

# Parse CLI arguments
while [[ $# -gt 0 ]]; do
  case "$1" in
    --use-cross)
      USE_CROSS=true
      shift
      ;;
    --all)
      BUILD_ALL=true
      shift
      ;;
    --out-dir)
      OUT_DIR="$2"
      shift 2
      ;;
    --help|-h)
      echo "Usage: $0 [--use-cross] [--all] [--out-dir <dir>] [targets...]"
      exit 0
      ;;
    *)
      SELECTED_TARGETS+=("$1")
      shift
      ;;
  esac
done

if [ "$BUILD_ALL" = true ]; then
  TARGETS=("${ALL_TARGETS[@]}")
elif [ ${#SELECTED_TARGETS[@]} -gt 0 ]; then
  TARGETS=("${SELECTED_TARGETS[@]}")
else
  # Default: build host target
  HOST_TARGET=$(rustc -vV | sed -n 's|host: ||p')
  TARGETS=("$HOST_TARGET")
fi

mkdir -p "${OUT_DIR}"

# Determine cargo target directory (support shared target dirs)
TARGET_BASE_DIR=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
if [ -z "$TARGET_BASE_DIR" ]; then
  TARGET_BASE_DIR="${PROJECT_ROOT}/target"
fi

BUILD_CMD="cargo"
if [ "$USE_CROSS" = true ]; then
  if ! command -v cross &>/dev/null; then
    echo "⚠️ 'cross' command not found. Installing via cargo..."
    cargo install cross --git https://github.com/cross-rs/cross
  fi
  BUILD_CMD="cross"
elif command -v cargo-zigbuild &>/dev/null; then
  # Automatically use cargo-zigbuild if available when cross compiling (e.g. on macOS compiling windows/linux)
  BUILD_CMD="cargo-zigbuild"
fi

echo "======================================================================="
echo "📦 Packaging ${APP_NAME} v${VERSION}"
echo "   Build tool : ${BUILD_CMD}"
echo "   Target dir : ${TARGET_BASE_DIR}"
echo "   Output dir : ${OUT_DIR}"
echo "   Targets    : ${TARGETS[*]}"
echo "======================================================================="

for TARGET in "${TARGETS[@]}"; do
  echo ""
  echo "🚀 Building for target: ${TARGET} ..."

  if [ "$BUILD_CMD" = "cargo" ]; then
    # Ensure rustup target is installed
    rustup target add "${TARGET}" 2>/dev/null || true
  fi

  # Execute build
  "${BUILD_CMD}" build --release --target "${TARGET}"

  BIN_NAME="${APP_NAME}"
  EXT="tar.gz"
  if [[ "${TARGET}" == *"windows"* ]]; then
    BIN_NAME="${APP_NAME}.exe"
    EXT="zip"
  fi

  BIN_SRC="${TARGET_BASE_DIR}/${TARGET}/release/${BIN_NAME}"
  if [ ! -f "${BIN_SRC}" ]; then
    # Fallback to local target if shared didn't catch it
    if [ -f "${PROJECT_ROOT}/target/${TARGET}/release/${BIN_NAME}" ]; then
      BIN_SRC="${PROJECT_ROOT}/target/${TARGET}/release/${BIN_NAME}"
    else
      echo "❌ Error: Binary not found at ${BIN_SRC}"
      exit 1
    fi
  fi

  STAGE_DIR="${OUT_DIR}/stage_${TARGET}"
  rm -rf "${STAGE_DIR}"
  mkdir -p "${STAGE_DIR}"

  cp "${BIN_SRC}" "${STAGE_DIR}/"
  cp "${PROJECT_ROOT}/README.md" "${STAGE_DIR}/" 2>/dev/null || true
  cp "${PROJECT_ROOT}/LICENSE" "${STAGE_DIR}/" 2>/dev/null || true

  ARCHIVE_NAME="${APP_NAME}-v${VERSION}-${TARGET}.${EXT}"
  ARCHIVE_PATH="${OUT_DIR}/${ARCHIVE_NAME}"

  echo "📦 Creating archive: ${ARCHIVE_NAME} ..."
  if [ "$EXT" = "zip" ]; then
    (cd "${STAGE_DIR}" && zip -q -9 -r "${ARCHIVE_PATH}" ./*)
  else
    tar -czf "${ARCHIVE_PATH}" -C "${STAGE_DIR}" .
  fi

  rm -rf "${STAGE_DIR}"

  # Generate SHA256 checksum
  echo "🔑 Generating SHA256..."
  if command -v sha256sum &>/dev/null; then
    (cd "${OUT_DIR}" && sha256sum "${ARCHIVE_NAME}" > "${ARCHIVE_NAME}.sha256")
  elif command -v shasum &>/dev/null; then
    (cd "${OUT_DIR}" && shasum -a 256 "${ARCHIVE_NAME}" > "${ARCHIVE_NAME}.sha256")
  fi

  echo "✅ Finished ${TARGET} -> ${ARCHIVE_PATH}"
done

echo ""
echo "======================================================================="
echo "🎉 All builds & packaging completed successfully!"
echo "📂 Artifacts available in: ${OUT_DIR}"
ls -lh "${OUT_DIR}"
echo "======================================================================="
