#!/usr/bin/env bash
# Boot an EXT4 fixture with real musl and glibc executables and runtimes.
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
export ELF_GREETING=Hello
result=0
./hello-musl musl || result=1
./hello-glibc glibc || result=1
./hello-glibc-exec glibc-exec || result=1
[ "$result" = 0 ] && echo elf-dynamic-test-ok
EOF

truncate -s 48M "$tmp/disk.img"
mkfs.ext4 -q -F -d "$tmp/root" "$tmp/disk.img"
timeout --kill-after=5 "${QEMU_TIMEOUT:-30}" "${QEMU:-qemu-system-riscv64}" \
    -machine virt -smp 1 -m 256M -nographic -bios default -kernel "$KERNEL" -no-reboot \
    -drive "file=$tmp/disk.img,if=none,format=raw,id=x0" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" ||
   ! grep -aqF 'elf-dynamic-test-ok' "$log" ||
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "动态 ELF 回归失败，见 $log"
fi
for libc in musl glibc glibc-exec; do
    grep -aqF "Hello, $libc!" "$log" || die "缺少 $libc 的输出，见 $log"
done
printf '动态 ELF 回归通过，日志 %s\n' "$log"
