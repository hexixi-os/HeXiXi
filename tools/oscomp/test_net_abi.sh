#!/usr/bin/env bash
# Exercise the IPv4 socket ABI, optionally with one local VirtIO TCP exchange.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

usage() {
    cat <<EOF
用法: $0 [--virtio] [--smp 1|2] [--arch] [riscv64|loongarch64|all] [日志文件或 all 的日志目录]
默认测试两个架构。--smp 仅用于 RISC-V；--virtio 额外验证宿主机回环到客机的 TCP 回显。
单架构可用 KERNEL、USER_BIN_DIR 和 QEMU 覆盖构建产物及模拟器。
EOF
}

arch=
output=
virtio=0
harts=${QEMU_SMP:-1}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --arch)
            [ "$#" -ge 2 ] && [ -z "$arch" ] || die "--arch 需要一个架构"
            arch=$2
            shift
            ;;
        --smp)
            [ "$#" -ge 2 ] || die "--smp 需要核数"
            harts=$2
            shift
            ;;
        --virtio) virtio=1 ;;
        -h|--help) usage; exit 0 ;;
        -*) die "不支持的参数 $1" ;;
        riscv64|loongarch64|all)
            [ -z "$arch" ] || die "重复的架构 $1"
            arch=$1
            ;;
        *)
            [ -z "$output" ] || die "不支持的参数 $1"
            output=$1
            ;;
    esac
    shift
done

arch=${arch:-all}
case "$arch" in
    riscv64|loongarch64|all) ;;
    *) die "不支持的架构 $arch" ;;
esac
case "$harts" in
    1|2) ;;
    *) die "仅支持 1 或 2 核" ;;
esac
if [ "$arch" = loongarch64 ] && [ "$harts" != 1 ]; then
    die "LoongArch 仅支持单核"
fi
if [ "$arch" = all ] && [ -n "${KERNEL:-}${USER_BIN_DIR:-}${QEMU:-}" ]; then
    die "使用 KERNEL、USER_BIN_DIR 或 QEMU 时需要指定单个架构"
fi

run_virtio() {
    python3 - "$1" "${QEMU_TIMEOUT:-45}" "${@:2}" <<'PY'
from pathlib import Path
import socket
import subprocess
import sys
import time

log, limit, *command = sys.argv[1:]
payload = b"pr19-virtio-echo"
loopback = "127.0.0.1"
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reserved:
    reserved.bind((loopback, 0))
    port = reserved.getsockname()[1]
command += ["-netdev", f"user,id=n0,restrict=on,hostfwd=tcp:{loopback}:{port}-10.0.2.15:42019"]
deadline = time.monotonic() + float(limit)


def remaining():
    return max(0, deadline - time.monotonic())


try:
    with Path(log).open("wb") as out, Path(log).open("rb") as serial:
        with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT) as qemu:
            try:
                recent = b""
                while b"[net-abi] VIRTIO READY" not in recent:
                    if qemu.poll() is not None:
                        raise RuntimeError("QEMU exited before the guest listened")
                    if remaining() == 0:
                        raise TimeoutError("guest did not listen before timeout")
                    recent = (recent + serial.read())[-4096:]
                    time.sleep(0.02)
                with socket.create_connection((loopback, port), timeout=remaining()) as conn:
                    conn.sendall(payload)
                    echo = bytearray()
                    while len(echo) < len(payload):
                        conn.settimeout(remaining())
                        chunk = conn.recv(len(payload) - len(echo))
                        if not chunk:
                            break
                        echo.extend(chunk)
                    if echo != payload:
                        raise RuntimeError(f"unexpected VirtIO echo {bytes(echo)!r}")
                    conn.shutdown(socket.SHUT_WR)
                if qemu.wait(timeout=remaining()) != 0:
                    raise RuntimeError(f"QEMU exited with status {qemu.returncode}")
            finally:
                if qemu.poll() is None:
                    qemu.terminate()
                    try:
                        qemu.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        qemu.kill()
except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
    sys.exit(f"VirtIO echo failed: {error}")
PY
}

run_arch() (
    local arch=$1 log=$2 harts=$3
    local tmp target bins kernel memory smp
    local -a platform netdev command
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/root" "$(dirname "$log")"

    case "$arch" in
        riscv64)
            target=riscv64gc-unknown-none-elf
            memory=256M
            platform=(-bios default -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0)
            netdev=(-device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1)
            if [ -z "${USER_BIN_DIR:-}" ]; then
                (cd "$REPO/user" && cargo build --locked --release --bin net_abi_test --bin oscomp_runner)
            fi
            if [ -z "${KERNEL:-}" ]; then
                smp=0
                if [ "$harts" = 2 ]; then smp=1; fi
                make -C "$REPO" all SMP="$smp" KERNEL_OUT="$tmp/kernel"
            fi
            ;;
        loongarch64)
            target=loongarch64-unknown-none-softfloat
            memory=1G
            platform=(-device virtio-blk-pci,drive=x0,disable-legacy=on,romfile=)
            netdev=(-device virtio-net-pci,netdev=n0,disable-legacy=on,romfile=)
            if [ -z "${USER_BIN_DIR:-}" ]; then
                (cd "$REPO/user" && RUSTC_BOOTSTRAP=user_lib cargo "+${LA_TOOLCHAIN:-1.98.1}" build --locked \
                    --release --target "$target" --bin net_abi_test --bin oscomp_runner)
            fi
            if [ -z "${KERNEL:-}" ]; then
                make -C "$REPO" kernel-la LA_KERNEL_OUT="$tmp/kernel"
            fi
            ;;
    esac

    bins=${USER_BIN_DIR:-$REPO/user/target/$target/release}
    kernel=${KERNEL:-$tmp/kernel}
    cp "$bins/net_abi_test" "$bins/oscomp_runner" "$tmp/root/"
    printf './net_abi_test\n' > "$tmp/root/basic_testcode.sh"
    if [ "$virtio" = 1 ]; then
        touch "$tmp/root/net-virtio"
    fi
    truncate -s 32M "$tmp/disk.img"
    mkfs.ext4 -q -F -b 4096 -d "$tmp/root" "$tmp/disk.img"

    command=("${QEMU:-qemu-system-$arch}" -machine virt -smp "$harts" -m "$memory"
        -nographic -monitor none -kernel "$kernel" -no-reboot -nic none
        -drive "file=$tmp/disk.img,if=none,format=raw,id=x0,readonly=on" "${platform[@]}")
    if [ "$virtio" = 1 ]; then
        run_virtio "$log" "${command[@]}" "${netdev[@]}" || die "VirtIO TCP 回显失败，见 $log"
    else
        timeout --kill-after=5 "${QEMU_TIMEOUT:-45}" "${command[@]}" \
            < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"
    fi

    if grep -aqE '\[kernel\] PANIC|Panicked at' "$log" ||
       ! grep -aqF '[net-abi] PASS' "$log" ||
       ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
        die "$arch 网络 ABI 回归失败，见 $log"
    fi
    if [ "$virtio" = 1 ] && ! grep -aqF '[net-abi] VIRTIO PASS' "$log"; then
        die "$arch VirtIO TCP 回显缺少客机完成标记，见 $log"
    fi
    printf '%s 网络 ABI 回归通过，日志 %s\n' "$arch" "$log"
)

if [ "$arch" = all ]; then
    run_arch riscv64 "${output:-$WORK}/net-abi-riscv64.log" "$harts"
    run_arch loongarch64 "${output:-$WORK}/net-abi-loongarch64.log" 1
else
    run_arch "$arch" "${output:-$WORK/net-abi-$arch.log}" "$harts"
fi
