#!/usr/bin/env bash
# Run the ELF loader regression on a purpose-built RISC-V EXT4 disk.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

log=${1:-$WORK/elf-demand.log}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$(dirname "$log")"

if [ -z "${USER_BIN_DIR:-}" ]; then
    (cd "$REPO/user" && cargo build --release --bin elf_demand_test --bin oscomp_runner)
    USER_BIN_DIR="$REPO/user/target/riscv64gc-unknown-none-elf/release"
fi
if [ -z "${KERNEL:-}" ]; then
    KERNEL="$tmp/kernel-rv"
    make -C "$REPO" all KERNEL_OUT="$KERNEL"
fi
cp "$USER_BIN_DIR/elf_demand_test" "$USER_BIN_DIR/oscomp_runner" "$tmp/root/"
printf './elf_demand_test\n' > "$tmp/root/basic_testcode.sh"
if [ "${ELF_STARTUP_ONLY:-0}" = 1 ]; then
    touch "$tmp/root/startup-only"
    expected=elf-startup-test-ok
else
    expected=elf-demand-test-ok
fi
python3 "$_ENV_DIR/tests/elf_fixture.py" "$tmp/root"
truncate -s 48M "$tmp/disk.img"
mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"

timeout --kill-after=5 "${QEMU_TIMEOUT:-30}" "${QEMU:-qemu-system-riscv64}" \
    -machine virt -smp 1 -m "${QEMU_MEM:-256M}" -nographic -bios default -kernel "$KERNEL" -no-reboot \
    -drive "file=$tmp/disk.img,if=none,format=raw,id=x0" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" || \
   ! grep -aqF "$expected" "$log" || \
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "ELF 回归失败，见 $log"
fi
printf 'ELF 回归通过，日志 %s\n' "$log"
