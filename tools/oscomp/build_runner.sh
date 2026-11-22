#!/usr/bin/env bash
# build_runner.sh — 构建用户态 oscomp_runner，并放到 $RUNNER（各组盘都要拷它进根目录）
#
# oscomp_runner 是内核启动的第一个进程（见 os/src/task/mod.rs 的 INITPROC）：
# 它在盘的根目录里找 *_testcode.sh，用盘里的 busybox sh 依次执行。
# 内核本身不需要重编译就能换 runner，但改了 user/src/bin/oscomp_runner.rs 后要重跑本脚本。
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

echo "== 构建 user（含 oscomp_runner）"
make -C "$REPO/user" elf >/dev/null
ELF="$REPO/user/target/riscv64gc-unknown-none-elf/release/oscomp_runner"
[ -f "$ELF" ] || die "没有产出 $ELF"

mkdir -p "$(dirname "$RUNNER")"
cp -f "$ELF" "$RUNNER"
echo "runner: $RUNNER ($(stat -c%s "$RUNNER") bytes)"
