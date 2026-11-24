#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

[ "$#" -eq 2 ] || die "用法: $0 <fixture-root> <串口日志>"
fixture=$1
log=$2
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$(dirname "$log")"
cp -R "$fixture/." "$tmp/root/"
if [ -z "${USER_BIN_DIR:-}" ]; then
    (cd "$REPO/user" && cargo build --release --bin oscomp_runner)
    USER_BIN_DIR="$REPO/user/target/riscv64gc-unknown-none-elf/release"
fi
if [ -z "${KERNEL:-}" ]; then
    KERNEL="$tmp/kernel-rv"
    make -C "$REPO" all KERNEL_OUT="$KERNEL"
fi
cp "$USER_BIN_DIR/oscomp_runner" "$tmp/root/"
cat > "$tmp/root/basic_testcode.sh" <<'EOF'
./pthread-musl && echo pthread-musl-ok
./pthread-glibc && echo pthread-glibc-ok
./busybox-dynamic sh -c './busybox-dynamic true' && echo dynamic-busybox-ok
EOF

truncate -s 32M "$tmp/disk.img"
mkfs.ext4 -q -F -d "$tmp/root" "$tmp/disk.img"
timeout --kill-after=5 "${QEMU_TIMEOUT:-45}" "${QEMU:-qemu-system-riscv64}" \
    -machine virt -smp 1 -m 256M -nographic -bios default -kernel "$KERNEL" -no-reboot \
    -snapshot -drive "file=$tmp/disk.img,if=none,format=raw,id=x0" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

for marker in pthread-musl-ok pthread-glibc-ok dynamic-busybox-ok; do
    grep -aqF "$marker" "$log" || die "缺少 $marker，见 $log"
done
if grep -aqF '[kernel] PANIC' "$log" || ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "pthread ABI 回归失败，见 $log"
fi
printf 'pthread ABI 回归通过，日志 %s\n' "$log"
