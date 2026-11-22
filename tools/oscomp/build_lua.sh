#!/usr/bin/env bash
# build_lua.sh — 交叉构建 riscv64 musl 静态非 PIE 的 lua，并打 lua 组测试盘
#
# 产物：$SUITE/lua/src/lua、$WORK/lua/disk.img
# 盘根目录：/lua  /date.lua ... /sort.lua（scripts/lua/*）  /busybox  /oscomp_runner
#
# 注意：oscomp_runner 对 lua 组有专门分支（盘里有 lua 和 date.lua 时，直接 fork+exec
# lua 去跑每个 .lua，不走 lua_testcode.sh —— 因为 busybox/test.sh 那条路会崩）。
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
need_suite; need_cc

make -C "$SUITE/lua" clean >/dev/null 2>&1 || true
make -C "$SUITE/lua" CC="$CC -static -no-pie" -j"$(nproc)" >"$WORK/lua-build.log" 2>&1 \
    || { mkdir -p "$WORK"; tail -20 "$WORK/lua-build.log" 2>/dev/null; die "lua 构建失败"; }
[ -f "$SUITE/lua/src/lua" ] || die "没有产出 lua"
echo "lua: $(stat -c%s "$SUITE/lua/src/lua") bytes"

ROOT="$WORK/lua/rootfs"
rm -rf "$ROOT"; mkdir -p "$ROOT"
install -m755 "$SUITE/lua/src/lua" "$ROOT/lua"
for f in "$SUITE"/scripts/lua/*; do install -m755 "$f" "$ROOT/$(basename "$f")"; done
stage_common "$ROOT"
mkimage "$ROOT" "$WORK/lua/disk.img" 32 LUA
