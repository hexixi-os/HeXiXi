#!/usr/bin/env bash
# build_busybox.sh — 交叉构建 riscv64 musl 静态非 PIE 的 busybox
#
# 产物：$SUITE/busybox/busybox（所有组盘的 stage_common 都从这里取）
#
# 说明：
#   - 配置用官方的 config/busybox-config-riscv64（Makefile.sub 同款）。
#   - 必须 -static -no-pie：rCore 不支持动态链接，也不做 PIE 重定位。
#   - 官方 sh 脚本的第一行是 `#!./busybox sh`，rCore 的 execve 不认 shebang，
#     所以 oscomp_runner 是显式 `busybox sh <脚本>` 去跑的。
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
need_suite; need_cc

cd "$SUITE/busybox"
cp -f "$SUITE/config/busybox-config-riscv64" .config
# 配置文件版本与源码不完全一致时 oscomp 的 make 会交互提问，喂回车走默认值
yes "" | make CC="$CC -static -no-pie" STRIP="$STRIP" oldconfig >/dev/null 2>&1 || true
make CC="$CC -static -no-pie" STRIP="$STRIP" -j"$(nproc)" >"$WORK/busybox-build.log" 2>&1 \
    || { mkdir -p "$WORK"; tail -30 "$WORK/busybox-build.log" 2>/dev/null; die "busybox 构建失败"; }
[ -f busybox ] || die "没有产出 busybox"
echo "busybox: $SUITE/busybox/busybox ($(stat -c%s busybox) bytes)"
file busybox | cut -c1-150
