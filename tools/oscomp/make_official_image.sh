#!/usr/bin/env bash
# make_official_image.sh — 生成 basic 组的官方布局 EXT4 测试盘（无分区表）
#
# 根目录布局（oscomp_runner 会找 basic_testcode.sh，再用盘里的 busybox sh 执行）：
#   /basic_testcode.sh  /basic/<各用例, run-all.sh, text.txt, mnt/>
#   /busybox  /oscomp_runner  /tmp  /var/tmp
#
# 前置：setup.sh、build_runner.sh、build_busybox.sh、build_tests.sh
# 产物：$WORK/official/disk.img
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
need_suite
[ -x "$WORK/basic/bin/run-all.sh" ] || [ -f "$WORK/basic/bin/run-all.sh" ] \
    || die "找不到 basic 用例，请先运行 tools/oscomp/build_tests.sh"

ROOT="$WORK/official/rootfs"
rm -rf "$ROOT"
mkdir -p "$ROOT/basic"
cp -r "$WORK/basic/bin/." "$ROOT/basic/"
chmod 755 "$ROOT/basic/run-all.sh"
install -m755 "$SUITE/scripts/basic/basic_testcode.sh" "$ROOT/basic_testcode.sh"
stage_common "$ROOT"

mkimage "$ROOT" "$WORK/official/disk.img" 64 OFFICIAL
