#!/usr/bin/env bash
# Boot a writable-overlay fixture over a read-only EXT4 image.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

log=${1:-$WORK/overlay-fs.log}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$(dirname "$log")"
if [ -z "${USER_BIN_DIR:-}" ]; then
    (cd "$REPO/user" && cargo build --release --bin overlay_fs_test --bin oscomp_runner)
    USER_BIN_DIR="$REPO/user/target/riscv64gc-unknown-none-elf/release"
fi
if [ -z "${KERNEL:-}" ]; then
    KERNEL="$tmp/kernel-rv"
    make -C "$REPO" all KERNEL_OUT="$KERNEL"
fi
cp "$USER_BIN_DIR/overlay_fs_test" "$USER_BIN_DIR/oscomp_runner" "$tmp/root/"
printf './overlay_fs_test\n' > "$tmp/root/basic_testcode.sh"
cp "$tmp/root/overlay_fs_test" "$tmp/root/exec-fixture"
printf lower > "$tmp/root/lower.bin"
printf untouched > "$tmp/root/untouched.bin"
truncate -s 32M "$tmp/disk.img"
mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"
sha256sum "$tmp/disk.img" > "$tmp/before.sha256"

timeout --kill-after=5 "${QEMU_TIMEOUT:-45}" "${QEMU:-qemu-system-riscv64}" \
    -machine virt -smp 1 -m 768M -nographic -bios default -kernel "$KERNEL" -no-reboot \
    -drive "file=$tmp/disk.img,if=none,format=raw,id=x0,readonly=on" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" || \
   ! grep -aqF 'overlay-fs-test-ok' "$log" || \
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "Overlay 回归失败，见 $log"
fi
sha256sum --status --check "$tmp/before.sha256" || die "EXT4 测试盘内容发生变化"
printf 'Overlay 回归通过，日志 %s\n' "$log"
