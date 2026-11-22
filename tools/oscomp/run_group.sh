#!/usr/bin/env bash
# run_group.sh — 用 QEMU 启动一张测试盘，保存串口日志，并汇总各组的 START/END 标记
#
# 用法：
#   tools/oscomp/run_group.sh <disk.img> [内存] [超时秒] [日志文件]
#   tools/oscomp/run_group.sh ~/oscomp-work/official/disk.img            # 默认 256M / 300s
#   tools/oscomp/run_group.sh ~/oscomp-work/lmbench/disk.img 1G 900
#
# 环境变量：KERNEL（默认 os/target/.../release/os.bin，改了内核先 `make -C os build`）
#          GDB=1   加 -s -S，QEMU 停在第一条指令等 gdb（另开终端 make -C os gdbclient）
#
# 内存要求（来自作者的记录）：lmbench 需要 1G；libcbench 用 256M；其余 256M 即可。
# 日志里 "Idle process exit with exit_code 0" 表示内核正常关机。
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

IMG=${1:-}; MEM=${2:-256M}; TMO=${3:-300}
[ -f "$IMG" ] || die "用法: $0 <disk.img> [内存] [超时秒] [日志]"
LOG=${4:-"$(dirname "$IMG")/run-$(date +%m%d-%H%M%S).log"}
KERNEL=${KERNEL:-$REPO/os/target/riscv64gc-unknown-none-elf/release/os.bin}
[ -f "$KERNEL" ] || die "找不到内核 $KERNEL，请先 make -C os build"

extra=()
[ "${GDB:-0}" = 1 ] && extra=(-s -S)

echo "== kernel: $KERNEL"
echo "== disk  : $IMG   mem=$MEM  timeout=${TMO}s"
echo "== log   : $LOG"
timeout "$TMO" qemu-system-riscv64 -machine virt -m "$MEM" -nographic -bios default \
    -kernel "$KERNEL" -no-reboot "${extra[@]}" \
    -drive file="$IMG",if=none,format=raw,id=x0 \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    < /dev/null > "$LOG" 2>&1
rc=$?

echo "== qemu exit=$rc $([ $rc = 124 ] && echo '(超时被杀)')   日志 $(wc -l < "$LOG") 行"
echo "== 组标记："
grep -a "OS COMP TEST GROUP" "$LOG" || echo "   （没有任何组标记）"
grep -a -m1 "Idle process exit" "$LOG" || echo "== 内核没有走到正常关机"
grep -a -cE "panic|Panicked" "$LOG" | sed 's/^/== panic 行数: /'
