#!/usr/bin/env bash
# Check the Linux ELF entry register/stack contract and RISC-V clone SP.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

[ "$#" -eq 2 ] || die "用法: $0 <riscv64|loongarch64> <串口日志>"
arch=$1
log=$2
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root/musl" "$(dirname "$log")"

case "$arch" in
    riscv64)
        target=riscv64gc-unknown-none-elf
        toolchain=nightly-2022-08-05
        memory=256M
        if [ -z "${KERNEL:-}" ]; then
            KERNEL="$tmp/kernel-rv"
            make -C "$REPO" all KERNEL_OUT="$KERNEL"
        fi
        platform=(-bios default -append oscomp.group=basic-musl
                  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0)
        ;;
    loongarch64)
        target=loongarch64-unknown-none-softfloat
        toolchain=${LA_TOOLCHAIN:-1.98.1}
        memory=1G
        if [ -z "${KERNEL:-}" ]; then
            KERNEL="$tmp/kernel-la"
            make -C "$REPO" kernel-la LA_KERNEL_OUT="$KERNEL"
        fi
        platform=(-monitor none -nic none -fw_cfg name=opt/hexixi/oscomp.group,string=basic-musl
                  -device virtio-blk-pci,drive=x0,disable-legacy=on,romfile=)
        ;;
    *) die "不支持的架构 $arch" ;;
esac

if [ -z "${PROBE_ELF:-}" ]; then
    PROBE_ELF="$tmp/exec_entry"
    rustc "+$toolchain" --edition 2021 --target "$target" -O -C panic=abort \
        -C "link-arg=-T$REPO/user/src/linker.ld" \
        "$REPO/tools/oscomp/tests/exec_entry.rs" -o "$PROBE_ELF"
fi
cp "$PROBE_ELF" "$tmp/root/musl/busybox"
ln "$tmp/root/musl/busybox" "$tmp/root/musl/exec_entry"
touch "$tmp/root/musl/basic_testcode.sh"
truncate -s 16M "$tmp/disk.img"
mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"

timeout --kill-after=5 "${QEMU_TIMEOUT:-45}" "${QEMU:-qemu-system-$arch}" \
    -machine virt -smp "${QEMU_SMP:-1}" -m "$memory" -nographic -kernel "$KERNEL" -no-reboot \
    -drive "file=$tmp/disk.img,if=none,format=raw,id=x0,readonly=on" "${platform[@]}" \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" ||
   ! grep -aqF 'exec-entry-test-ok' "$log" ||
   ! grep -aqF '[oscomp] result basic-musl exit=0' "$log" ||
   [ "$(grep -acF '[oscomp] running /' "$log")" -ne 1 ] ||
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "ELF 入口回归失败，见 $log"
fi
printf 'ELF 入口回归通过，日志 %s\n' "$log"
