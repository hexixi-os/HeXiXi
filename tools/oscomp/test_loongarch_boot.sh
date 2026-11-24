#!/usr/bin/env bash
# Check direct ELF boot, read-only VirtIO PCI / EXT4, and userspace execution.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

log=${1:-$WORK/loongarch-boot.log}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$(dirname "$log")"

if [ -z "${KERNEL:-}" ]; then
    KERNEL="$tmp/kernel-la"
    make -C "$REPO" kernel-la LA_KERNEL_OUT="$KERNEL"
fi
if [ -z "${USER_BIN_DIR:-}" ]; then
    (cd "$REPO/user" && RUSTC_BOOTSTRAP=user_lib cargo "+${LA_TOOLCHAIN:-1.98.1}" build --locked --release \
        --target loongarch64-unknown-none-softfloat --bin hello_world --bin oscomp_runner)
    USER_BIN_DIR="$REPO/user/target/loongarch64-unknown-none-softfloat/release"
fi
if [ -z "${CLONE_TEST_ELF:-}" ]; then
    CLONE_TEST_ELF="$tmp/loongarch_clone"
    rustc "+${LA_TOOLCHAIN:-1.98.1}" --edition 2021 --target loongarch64-unknown-none-softfloat \
        -O -C panic=abort -C "link-arg=-T$REPO/user/src/linker.ld" \
        "$REPO/tools/oscomp/tests/loongarch_clone.rs" -o "$CLONE_TEST_ELF"
fi
cp "$USER_BIN_DIR/hello_world" "$USER_BIN_DIR/oscomp_runner" "$tmp/root/"
cp "$CLONE_TEST_ELF" "$tmp/root/loongarch_clone"
printf './hello_world\n./loongarch_clone\n' > "$tmp/root/basic_testcode.sh"
truncate -s 16M "$tmp/disk.img"
mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"

timeout --kill-after=5 "${QEMU_TIMEOUT:-30}" "${QEMU:-qemu-system-loongarch64}" \
    -machine virt -m 1G -smp 1 -nographic -monitor none -nic none -no-reboot \
    -kernel "$KERNEL" -drive "file=$tmp/disk.img,if=none,format=raw,id=x0,readonly=on" \
    -device virtio-blk-pci,drive=x0,disable-legacy=on,romfile= \
    < /dev/null > "$log" 2>&1 || die "LoongArch64 QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" ||
   ! grep -aqF 'Hello world from user mode program!' "$log" ||
   ! grep -aqF 'loongarch-clone-ok' "$log" ||
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "LoongArch64 启动回归失败，见 $log"
fi
printf 'LoongArch64 启动回归通过，日志 %s\n' "$log"
