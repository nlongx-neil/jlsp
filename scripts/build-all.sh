#!/usr/bin/env bash
# ==============================================================================
# jlsp Multi-Platform & Multi-Architecture Binary Release Packaging Script
# ==============================================================================
# 特性要求：
# 1. 支持各系统 (macOS / Linux / Windows) 及对应 CPU 架构 (x86_64 / arm64)；
# 2. Linux 平台强制纯静态链接 (musl libc)，完全杜绝运行时对系统 gcc、glibc、
#    libgcc_s.so、libstdc++.so 等动态链接库的依赖，在 Alpine、Debian、CentOS、
#    Ubuntu 等任何 Linux 发行版及最小容器中开箱即用；
# 3. 产出纯净的单文件二进制执行文件，以及对应压缩归档包和 sha256 校验和。
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

APP_NAME="jlsp"
VERSION=$(grep -m1 '^version = ' Cargo.toml | cut -d '"' -f2 || echo "0.1.0")
OUT_DIR="${PROJECT_ROOT}/dist"
RAW_BIN_DIR="${OUT_DIR}/binaries"

# 目标架构矩阵与命名映射配置
# 格式: target_triple | os_name | arch_name | output_binary_name | archive_type
TARGET_SPECS=(
  # macOS (Mach-O 64-bit)
  "aarch64-apple-darwin|darwin|arm64|jlsp-darwin-arm64|tar.gz"
  "x86_64-apple-darwin|darwin|x86_64|jlsp-darwin-x86_64|tar.gz"

  # Linux (纯静态 MUSL 编译，0 动态依赖，无 glibc / libgcc / gcc 需求)
  "x86_64-unknown-linux-musl|linux|x86_64|jlsp-linux-x86_64|tar.gz"
  "aarch64-unknown-linux-musl|linux|arm64|jlsp-linux-arm64|tar.gz"

  # Windows (PE32+ console)
  "x86_64-pc-windows-gnu|windows|x86_64|jlsp-windows-x86_64.exe|zip"
)

SELECTED_TARGETS=()
SHOW_HELP=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out-dir)
      OUT_DIR="$2"
      RAW_BIN_DIR="${OUT_DIR}/binaries"
      shift 2
      ;;
    --help|-h)
      SHOW_HELP=true
      shift
      ;;
    *)
      SELECTED_TARGETS+=("$1")
      shift
      ;;
  esac
done

if [ "$SHOW_HELP" = true ]; then
  cat << 'EOF'
用法:
  ./scripts/build-all.sh [选项] [target_triple...]

选项:
  --out-dir <目录>    指定构建和打包输出目录 (默认: ./dist)
  -h, --help          查看使用帮助

默认行为:
  编译并打包全平台全架构二进制：
  - macOS (Apple Silicon arm64 / Intel x86_64)
  - Linux (x86_64 / aarch64, 纯静态 musl 编译，无 gcc/glibc 运行时依赖)
  - Windows (x86_64, .exe)

示例:
  ./scripts/build-all.sh
  ./scripts/build-all.sh x86_64-unknown-linux-musl
  ./scripts/build-all.sh --out-dir ./release-artifacts
EOF
  exit 0
fi

mkdir -p "${OUT_DIR}" "${RAW_BIN_DIR}"

# 获取共享构建缓存或局部 target 路径
TARGET_BASE_DIR=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
if [ -z "$TARGET_BASE_DIR" ]; then
  TARGET_BASE_DIR="${PROJECT_ROOT}/target"
fi

# 检查/选择最佳交叉构建编译器
if command -v cargo-zigbuild &>/dev/null; then
  BUILD_CMD="cargo-zigbuild"
elif command -v cross &>/dev/null; then
  BUILD_CMD="cross"
else
  BUILD_CMD="cargo"
fi

# 确保在 Linux 静态编译中强制静态链接 C/C++ 运行时
export RUSTFLAGS="-C target-feature=+crt-static ${RUSTFLAGS:-}"

echo "======================================================================="
echo "📦 正在打包 ${APP_NAME} v${VERSION} 全平台多架构原生二进制"
echo "   编译工具   : ${BUILD_CMD}"
echo "   目标构建目录: ${TARGET_BASE_DIR}"
echo "   输出产物目录: ${OUT_DIR}"
echo "   静态单文件  : ${RAW_BIN_DIR}"
echo "======================================================================="

SHA_FILE="${OUT_DIR}/sha256sums.txt"
rm -f "${SHA_FILE}"

for spec in "${TARGET_SPECS[@]}"; do
  IFS='|' read -r target os_name arch_name out_bin_name archive_ext <<< "$spec"

  # 如果指定了目标，则过滤
  if [ ${#SELECTED_TARGETS[@]} -gt 0 ]; then
    MATCH=false
    for st in "${SELECTED_TARGETS[@]}"; do
      if [[ "$st" == "$target" || "$st" == "$os_name" || "$st" == "$arch_name" || "$st" == "$out_bin_name" ]]; then
        MATCH=true
        break
      fi
    done
    if [ "$MATCH" = false ]; then
      continue
    fi
  fi

  echo ""
  echo "🚀 [${os_name}-${arch_name}] 正在编译目标: ${target} ..."

  # 如果使用 cargo，自动补充 rustup target
  if [ "$BUILD_CMD" != "cross" ]; then
    rustup target add "${target}" 2>/dev/null || true
  fi

  # 执行发布版编译
  if [[ "${target}" == *"linux-musl"* ]]; then
    # 强制静态 Musl libc，彻底杜绝 gcc/glibc/libgcc_s.so 依赖
    "${BUILD_CMD}" build --release --target "${target}"
  else
    "${BUILD_CMD}" build --release --target "${target}"
  fi

  # 寻找生成二进制
  SRC_BIN_NAME="${APP_NAME}"
  if [[ "${target}" == *"windows"* ]]; then
    SRC_BIN_NAME="${APP_NAME}.exe"
  fi

  BIN_FILE="${TARGET_BASE_DIR}/${target}/release/${SRC_BIN_NAME}"
  if [ ! -f "${BIN_FILE}" ]; then
    if [ -f "${PROJECT_ROOT}/target/${target}/release/${SRC_BIN_NAME}" ]; then
      BIN_FILE="${PROJECT_ROOT}/target/${target}/release/${SRC_BIN_NAME}"
    else
      echo "❌ 错误: 未找到目标产物 ${BIN_FILE}"
      exit 1
    fi
  fi

  # 1. 拷贝独立纯二进制到 binaries/ 目录
  DEST_RAW_BIN="${RAW_BIN_DIR}/${out_bin_name}"
  cp "${BIN_FILE}" "${DEST_RAW_BIN}"
  chmod +x "${DEST_RAW_BIN}"

  # 验证 Linux 二进制静态无依赖性
  if [[ "${target}" == *"linux"* ]]; then
    echo "🔍 检查 Linux 动态依赖 ..."
    if command -v readelf &>/dev/null; then
      if readelf -d "${DEST_RAW_BIN}" | grep -q 'NEEDED'; then
        echo "⚠️ 警告: ${out_bin_name} 检测到动态库依赖！"
      else
        echo "✅ ${out_bin_name}: 纯静态二进制验证通过 (无任何外部 gcc / libc 依赖)"
      fi
    elif command -v strings &>/dev/null; then
      if strings "${DEST_RAW_BIN}" | grep -E "ld-linux|libgcc_s" &>/dev/null; then
        echo "⚠️ 警告: ${out_bin_name} 可能包含动态符号！"
      else
        echo "✅ ${out_bin_name}: 纯静态二进制验证通过 (无 ld-linux / libgcc_s 依赖)"
      fi
    fi
  fi

  # 2. 制作标准发布压缩包 (包含可执行文件、README 及 LICENSE)
  STAGE_DIR="${OUT_DIR}/stage_${target}"
  rm -rf "${STAGE_DIR}"
  mkdir -p "${STAGE_DIR}"

  cp "${BIN_FILE}" "${STAGE_DIR}/${SRC_BIN_NAME}"
  cp "${PROJECT_ROOT}/README.md" "${STAGE_DIR}/" 2>/dev/null || true
  cp "${PROJECT_ROOT}/LICENSE" "${STAGE_DIR}/" 2>/dev/null || true

  ARCHIVE_NAME="${APP_NAME}-v${VERSION}-${target}.${archive_ext}"
  ARCHIVE_PATH="${OUT_DIR}/${ARCHIVE_NAME}"

  echo "📦 打包压缩文件: ${ARCHIVE_NAME} ..."
  if [ "${archive_ext}" = "zip" ]; then
    (cd "${STAGE_DIR}" && zip -q -9 -r "${ARCHIVE_PATH}" ./*)
  else
    tar -czf "${ARCHIVE_PATH}" -C "${STAGE_DIR}" .
  fi
  rm -rf "${STAGE_DIR}"

  # 3. 计算 SHA256 校验码
  if command -v sha256sum &>/dev/null; then
    (cd "${OUT_DIR}" && sha256sum "${ARCHIVE_NAME}" >> "${SHA_FILE}")
    (cd "${RAW_BIN_DIR}" && sha256sum "${out_bin_name}" >> "${SHA_FILE}")
  elif command -v shasum &>/dev/null; then
    (cd "${OUT_DIR}" && shasum -a 256 "${ARCHIVE_NAME}" >> "${SHA_FILE}")
    (cd "${RAW_BIN_DIR}" && shasum -a 256 "${out_bin_name}" >> "${SHA_FILE}")
  fi

  echo "✅ 完成: ${out_bin_name} -> ${ARCHIVE_NAME}"
done

echo ""
echo "======================================================================="
echo "🎉 全平台与架构编译打包全部顺利完成！"
echo ""
echo "📁 独立二进制目录 (${RAW_BIN_DIR}):"
ls -lh "${RAW_BIN_DIR}"
echo ""
echo "📁 压缩发布包目录 (${OUT_DIR}):"
ls -lh "${OUT_DIR}"/*.tar.gz "${OUT_DIR}"/*.zip 2>/dev/null || true
echo ""
echo "🔑 SHA256 校验文件: ${SHA_FILE}"
echo "======================================================================="
