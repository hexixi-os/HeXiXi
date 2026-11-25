#!/usr/bin/env bash
# Run the Pipe wait regression, or measure scheduler dispatches in a fixed workload.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

log=${1:-$WORK/pipe-wait.log}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$(dirname "$log")"
arch=${ARCH:-riscv64}
case $arch in
    riscv64)
        target=riscv64gc-unknown-none-elf
        smp=${QEMU_SMP:-2}
        qemu_args=(-machine virt -smp "$smp" -m 256M -bios default)
        device=virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
        ;;
    loongarch64)
        target=loongarch64-unknown-none-softfloat
        qemu_args=(-machine virt -smp 1 -m 1G -monitor none -nic none)
        device=virtio-blk-pci,drive=x0,disable-legacy=on,romfile=
        ;;
    *) die "unsupported ARCH: $arch" ;;
esac

if [ -z "${USER_BIN_DIR:-}" ]; then
    if [ "$arch" = loongarch64 ]; then
        (cd "$REPO/user" && RUSTC_BOOTSTRAP=user_lib cargo "+${LA_TOOLCHAIN:-1.98.1}" \
            build --locked --release --target "$target" --bin pipe_wait_test)
    else
        (cd "$REPO/user" && cargo build --locked --release --bin pipe_wait_test)
    fi
    USER_BIN_DIR="$REPO/user/target/$target/release"
fi
if [ -z "${KERNEL:-}" ]; then
    KERNEL="$tmp/kernel"
    if [ "$arch" = loongarch64 ]; then
        make -C "$REPO" kernel-la LA_KERNEL_OUT="$KERNEL" SCHED_SWITCH_STATS="${PIPE_BENCH:-0}"
    else
        smp_feature=1
        if [ "$smp" = 1 ]; then smp_feature=0; fi
        make -C "$REPO" all SMP="$smp_feature" KERNEL_OUT="$KERNEL" SCHED_SWITCH_STATS="${PIPE_BENCH:-0}"
    fi
fi

cp "$USER_BIN_DIR/pipe_wait_test" "$tmp/root/initproc"
marker=pipe-wait-test-ok
if [ "${PIPE_BENCH:-0}" = 1 ]; then
    touch "$tmp/root/pipe_wait_bench"
    marker=pipe-wait-bench-ok
fi
truncate -s 32M "$tmp/disk.img"
mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"

timeout --kill-after=5 "${QEMU_TIMEOUT:-30}" "${QEMU:-qemu-system-$arch}" \
    "${qemu_args[@]}" -nographic -kernel "$KERNEL" -no-reboot \
    -drive "file=$tmp/disk.img,if=none,format=raw,id=x0,readonly=on" -device "$device" \
    < /dev/null > "$log" 2>&1 || die "QEMU did not exit successfully, see $log"

if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" || \
   ! grep -aqF "$marker" "$log" || \
   ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
    die "Pipe wait regression failed, see $log"
fi
if [ "${PIPE_BENCH:-0}" = 1 ]; then
    grep -aF '[sched] task_dispatches=' "$log" || die "kernel was built without scheduler statistics"
fi
printf '%s (%s) %s\n' "$marker" "$arch" "$log"
