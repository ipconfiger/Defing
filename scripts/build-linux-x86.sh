#!/usr/bin/env bash
# 交叉编译 Linux x86-64 (glibc) 可执行文件并复制到 bin/。
#
# 核心指令：cargo zigbuild --release --target x86_64-unknown-linux-gnu
# 依赖：cargo-zigbuild（cargo install cargo-zigbuild）、zig（https://ziglang.org）、rustup
#
# 产物：bin/dsh-linux-x86_64（静态链接 glibc；可用 ldd/file 验证）
# 覆盖：CARGO_TARGET_DIR（默认 $REPO/server/target）、CARGO_HOME（缺省 rustup 默认）
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
TARGET=x86_64-unknown-linux-gnu
CRATE=dsh-cli
BIN_NAME=dsh
OUT="$REPO/bin"

# ---------- 1. 依赖检查 ----------
for c in cargo-zigbuild zig rustup; do
  command -v "$c" >/dev/null 2>&1 || { echo "✗ 缺少依赖：$c (cargo install cargo-zigbuild / ziglang.org / rustup.rs)"; exit 1; }
done

# ---------- 2. 安装 Rust 交叉目标（幂等） ----------
rustup target add "$TARGET" >/dev/null 2>&1 || { echo "✗ rustup target add $TARGET 失败"; exit 1; }

# ---------- 3. 交叉编译（--release） ----------
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/server/target}"
cd "$REPO/server"
echo "== cargo zigbuild --release --target $TARGET (crate: $CRATE)=="
cargo zigbuild --release --target "$TARGET" -p "$CRATE"

# ---------- 4. 复制产物到 bin/ ----------
SRC="$CARGO_TARGET_DIR/$TARGET/release/$BIN_NAME"
if [ ! -f "$SRC" ]; then
  echo "✗ 编译产物未找到：$SRC"
  exit 1
fi
mkdir -p "$OUT"
cp "$SRC" "$OUT/$BIN_NAME-linux-x86_64"
chmod +x "$OUT/$BIN_NAME-linux-x86_64"
echo "✅ 已复制到 $OUT/$BIN_NAME-linux-x86_64"
if command -v file >/dev/null 2>&1; then file "$OUT/$BIN_NAME-linux-x86_64"; fi
echo "== 完成：$(ls -lh "$OUT/$BIN_NAME-linux-x86_64" | awk '{print $5}') =="
