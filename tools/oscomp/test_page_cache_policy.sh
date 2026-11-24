#!/usr/bin/env bash
# Run one page-cache workload from a fresh RISC-V boot.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

[ "$#" -eq 2 ] || die "用法: $0 <mixed32|mixed128|sequential|reuse|revisit|busybox> <串口日志>"
scenario=$1
case "$scenario" in
    mixed32|mixed128|sequential|reuse|revisit|busybox) ;;
    *) die "未知负载 $scenario" ;;
esac
policy=${PAGE_CACHE_POLICY:-}
case "$policy" in
    ''|none|lru|adaptive) ;;
    *) die "未知页缓存策略 $policy" ;;
esac
log=$2
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$(dirname "$log")"

if [ -z "${USER_BIN_DIR:-}" ]; then
    (cd "$REPO/user" && cargo build --release --bin page_cache_probe --bin oscomp_runner)
    USER_BIN_DIR="$REPO/user/target/riscv64gc-unknown-none-elf/release"
fi
if [ -z "${KERNEL:-}" ]; then
    KERNEL="$tmp/kernel-rv"
    policy=${policy:-lru}
    make -C "$REPO" all KERNEL_OUT="$KERNEL" PAGE_CACHE_POLICY="$policy"
fi
image=${PAGE_CACHE_IMAGE:-$tmp/disk.img}
if [ ! -f "$image" ]; then
    cp "$USER_BIN_DIR/page_cache_probe" "$USER_BIN_DIR/oscomp_runner" "$tmp/root/"
    if [ "$scenario" = busybox ]; then
        [ -f "${PAGE_CACHE_BUSYBOX:-}" ] || die "请用 PAGE_CACHE_BUSYBOX 指定 RISC-V BusyBox ELF"
        cp "$PAGE_CACHE_BUSYBOX" "$tmp/root/page-cache-busybox"
    fi
    printf './page_cache_probe\n' > "$tmp/root/basic_testcode.sh"
    printf '%s\n' "$scenario" > "$tmp/root/scenario"
    python3 - "$tmp/root" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
for path, pages, marker in [("cold.bin", 1024, 0x10000), ("hot.bin", 128, 0x20000)]:
    with (root / path).open("wb") as file:
        for page in range(pages):
            file.write((marker + page).to_bytes(4, "little") * 1024)
PY
    truncate -s 32M "$tmp/disk.img"
    mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"
    if [ "$image" != "$tmp/disk.img" ]; then
        mkdir -p "$(dirname "$image")"
        cp -n "$tmp/disk.img" "$image"
    fi
fi

timeout --kill-after=5 "${QEMU_TIMEOUT:-45}" "${QEMU:-qemu-system-riscv64}" \
    -machine virt -smp 1 -m "${QEMU_MEM:-128M}" -nographic -bios default -kernel "$KERNEL" -no-reboot -snapshot \
    -drive "file=${image//,/,,},if=none,format=raw,id=x0" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" || \
   ! grep -aqF "page-cache-result scenario=$scenario " "$log" || \
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "页缓存实验失败，见 $log"
fi
result=$(grep -aF "page-cache-result scenario=$scenario " "$log" | tr -d '\r')
if [ -n "$policy" ] && ! printf '%s\n' "$result" | grep -iqF " policy=$policy "; then
    die "内核实际策略与 $policy 不符，见 $log"
fi
printf '%s\n' "$result"
