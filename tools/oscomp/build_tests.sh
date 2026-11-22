#!/usr/bin/env bash
# build_tests.sh — 编译 basic 组的测试用例（官方 CHAPTER=7 那套，共 32 个系统调用用例）
#
# 产物：$WORK/basic/bin/   各测试二进制 + run-all.sh + text.txt + mnt/test_mount
#
# 做法：把 $SUITE/basic/user 拷到 $WORK/basic/src 里再构建，不弄脏测试套件源码
# （官方 CMakeLists 会往源码树写 lib/syscall_ids.h）。
# 编译器用 musl 交叉 gcc 只是当个 riscv64 gcc 用：用例自带 -nostdlib -nostdinc，
# 直接发系统调用，不依赖任何 libc。
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
need_suite; need_cc
[ -x "$CMAKE_BIN/cmake" ] || die "找不到 cmake，请先运行 tools/oscomp/setup.sh"

SRC="$WORK/basic/src"
OUT="$WORK/basic/bin"
rm -rf "$SRC" "$OUT"
mkdir -p "$SRC" "$OUT"
cp -r "$SUITE/basic/user/." "$SRC/"

cd "$SRC"
make binary ARCH=riscv64 PREFIX="$PREFIX" CHAPTER=7 > "$WORK/basic/build.log" 2>&1 \
    || { tail -30 "$WORK/basic/build.log"; die "basic 构建失败（完整日志 $WORK/basic/build.log）"; }

cp -r "$SRC/build/riscv64/." "$OUT/"
n=$(find "$OUT" -maxdepth 1 -type f -executable ! -name '*.sh' | wc -l)
echo "basic: $n 个可执行用例 -> $OUT"
# 对照 run-all.sh 里列出的用例，缺哪个就报哪个
missing=0
for t in $(sed -n '/^tests="/,/^"/p' "$OUT/run-all.sh" | grep -vE '^tests=|^"'); do
    [ -x "$OUT/$t" ] || { echo "  缺少用例: $t"; missing=1; }
done
[ "$missing" = 0 ] && echo "run-all.sh 里的用例全部已构建"
