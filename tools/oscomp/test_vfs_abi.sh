#!/usr/bin/env bash
# Exercise the VFS against both on-disk symlinks and writable overlay nodes.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

[ "$#" -eq 2 ] || die "用法: $0 <riscv64|loongarch64> <串口日志>"
arch=$1
log=$2
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root/fixture/data" "$tmp/root/lib" "$(dirname "$log")"

case "$arch" in
    riscv64)
        target=riscv64gc-unknown-none-elf
        if [ -z "${USER_BIN_DIR:-}" ]; then
            (cd "$REPO/user" && cargo build --locked --release --bin vfs_abi_test --bin oscomp_runner)
        fi
        if [ -z "${KERNEL:-}" ]; then
            KERNEL="$tmp/kernel-rv"
            make -C "$REPO" all KERNEL_OUT="$KERNEL"
        fi
        memory=256M
        platform=(-bios default -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0)
        ;;
    loongarch64)
        target=loongarch64-unknown-none-softfloat
        if [ -z "${USER_BIN_DIR:-}" ]; then
            (cd "$REPO/user" && RUSTC_BOOTSTRAP=user_lib cargo "+${LA_TOOLCHAIN:-1.98.1}" build --locked \
                --release --target "$target" --bin vfs_abi_test --bin oscomp_runner)
        fi
        if [ -z "${KERNEL:-}" ]; then
            KERNEL="$tmp/kernel-la"
            make -C "$REPO" kernel-la LA_KERNEL_OUT="$KERNEL"
        fi
        memory=1G
        platform=(-monitor none -nic none -device virtio-blk-pci,drive=x0,disable-legacy=on,romfile=)
        ;;
    *) die "不支持的架构 $arch" ;;
esac

bins=${USER_BIN_DIR:-$REPO/user/target/$target/release}
cp "$bins/vfs_abi_test" "$bins/oscomp_runner" "$tmp/root/"
printf './vfs_abi_test\n' > "$tmp/root/basic_testcode.sh"
printf payload > "$tmp/root/fixture/data/value"
printf lib-ok > "$tmp/root/lib/runtime.marker"
printf UTF-8 > "$tmp/root/fixture/data/多字节"
ln -s 'data/value' "$tmp/root/fixture/short"
ln -s 'data/多字节' "$tmp/root/fixture/unicode"
ln -s '/fixture/data' "$tmp/root/fixture/dir-link"
edge_target=$(printf './%.0s' {1..25})data/value
ln -s "$edge_target" "$tmp/root/fixture/edge"
long_target=$(printf './%.0s' {1..32})data/value
ln -s "$long_target" "$tmp/root/fixture/long"
truncate -s 32M "$tmp/disk.img"
mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"

timeout --kill-after=5 "${QEMU_TIMEOUT:-45}" "${QEMU:-qemu-system-$arch}" \
    -machine virt -smp "${QEMU_SMP:-1}" -m "$memory" -nographic -kernel "$KERNEL" -no-reboot \
    -drive "file=$tmp/disk.img,if=none,format=raw,id=x0,readonly=on" "${platform[@]}" \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" ||
   ! grep -aqF 'vfs-abi-test-ok' "$log" ||
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "VFS ABI 回归失败，见 $log"
fi
printf 'VFS ABI 回归通过，日志 %s\n' "$log"
