#!/usr/bin/env bash
# Exercise the RISC-V write, namespace and clone ABI on an EXT4 fixture.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

log=${1:-$WORK/basic-abi.log}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root/basic/lower-dir" "$tmp/root/musl" "$(dirname "$log")"

if [ -z "${USER_BIN_DIR:-}" ]; then
    (cd "$REPO/user" && cargo build --release --bin basic_abi_test --bin oscomp_runner)
    USER_BIN_DIR="$REPO/user/target/riscv64gc-unknown-none-elf/release"
fi
if [ -z "${KERNEL:-}" ]; then
    KERNEL="$tmp/kernel-rv"
    make -C "$REPO" all KERNEL_OUT="$KERNEL"
fi
cp "$USER_BIN_DIR/basic_abi_test" "$USER_BIN_DIR/oscomp_runner" "$tmp/root/"
printf './basic_abi_test\n' > "$tmp/root/basic_testcode.sh"
printf 'lower' > "$tmp/root/basic/lower-file"
printf 'regular file' > "$tmp/root/basic/mnt"
printf '#!/no-such-interpreter\n' > "$tmp/root/bad-shebang"
printf 'not an ELF' > "$tmp/root/musl/busybox"
truncate -s 32M "$tmp/disk.img"
mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"

timeout --kill-after=5 "${QEMU_TIMEOUT:-30}" "${QEMU:-qemu-system-riscv64}" \
    -machine virt -smp 1 -m 256M -nographic -bios default -kernel "$KERNEL" -no-reboot \
    -drive "file=$tmp/disk.img,if=none,format=raw,id=x0" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" || \
   ! grep -aqF 'basic-abi-test-ok' "$log" || \
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "basic ABI 回归失败，见 $log"
fi
printf 'basic ABI 回归通过，日志 %s\n' "$log"
