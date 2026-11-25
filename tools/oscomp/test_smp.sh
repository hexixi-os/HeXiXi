#!/usr/bin/env bash
# Boot a RISC-V SMP kernel and check cross-hart CoW, mmap and pipe I/O.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

log=${1:-$WORK/smp.log}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$(dirname "$log")"

if [ -z "${USER_BIN_DIR:-}" ]; then
    (cd "$REPO/user" && cargo build --release --bin smp_test --bin oscomp_runner)
    USER_BIN_DIR="$REPO/user/target/riscv64gc-unknown-none-elf/release"
fi
if [ -z "${KERNEL:-}" ]; then
    KERNEL="$tmp/kernel-rv"
    make -C "$REPO" all SMP=1 KERNEL_OUT="$KERNEL"
fi
cp "$USER_BIN_DIR/smp_test" "$USER_BIN_DIR/oscomp_runner" "$tmp/root/"
printf './smp_test\n' > "$tmp/root/basic_testcode.sh"
truncate -s 32M "$tmp/disk.img"
mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"

timeout --kill-after=5 "${QEMU_TIMEOUT:-45}" "${QEMU:-qemu-system-riscv64}" \
    -machine virt -smp "${QEMU_SMP:-2}" -m 256M -nographic -bios default -kernel "$KERNEL" -no-reboot \
    -drive "file=$tmp/disk.img,if=none,format=raw,id=x0,readonly=on" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" || \
   ! grep -aqF 'smp-test-ok' "$log" || \
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "SMP 回归失败，见 $log"
fi
printf 'SMP 回归通过，日志 %s\n' "$log"
