#!/usr/bin/env bash
# 用原样的 RISC-V64 评测盘运行 basic-musl，保存运行信息、串口和判分。
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

[ "$#" -eq 2 ] || die "用法: $0 <原版 RISC-V64 EXT4 镜像> <结果目录>"
image=$1
[ -f "$image" ] || die "找不到镜像 $image"
image=$(cd "$(dirname "$image")" && pwd)/$(basename "$image")
need_suite
judge="$SUITE/basic/user/src/oscomp/test_runner.py"
[ -f "$judge" ] || die "找不到 basic 官方判分器 $judge"
suite_commit=$(git -C "$SUITE" rev-parse --verify HEAD)
suite_changes=$(git -C "$SUITE" status --porcelain)
checkout_commit=$(git -C "$REPO" rev-parse HEAD)
source_changes=$(git -C "$REPO" status --porcelain)

qemu=${QEMU:-qemu-system-riscv64}
memory=${QEMU_MEM:-1G}
time_limit=${QEMU_TIMEOUT:-200}
for command in "$qemu" timeout sha256sum truncate python3; do
    command -v "$command" >/dev/null || die "找不到 $command"
done

hash_file() {
    sha256sum < "$1" | cut -d' ' -f1
}

checkout_state=clean
suite_state=clean
[ -z "$source_changes" ] || checkout_state=dirty
[ -z "$suite_changes" ] || suite_state=dirty

mkdir -p "$2"
output=$(cd "$2" && pwd)
run=$(mktemp -d "$output/run.XXXXXXXX")
meta="$run/run.meta"
serial="$run/serial.log"
basic="$run/basic-musl.log"
scratch="$run/disk.img"
printf '评测记录 %s\n' "$run"
trap 'rc=$?; printf "run_exit=%s\n" "$rc" >> "$meta"; rm -f "$scratch"' EXIT

judge_hash=$(hash_file "$judge")
image_hash=$(hash_file "$image")
qemu_version=$("$qemu" --version | sed -n '1p')
{
    printf 'checkout_commit=%s\ncheckout_state=%s\n' "$checkout_commit" "$checkout_state"
    printf 'suite_commit=%s\nsuite_state=%s\n' "$suite_commit" "$suite_state"
    printf 'judge_sha256=%s\n' "$judge_hash"
    printf 'qemu_version=%s\n' "$qemu_version"
    printf 'image=%q\n' "$image"
    printf 'image_sha256=%s\n' "$image_hash"
} > "$meta"

kernel=${KERNEL:-$run/kernel-rv}
if [ -z "${KERNEL:-}" ]; then
    printf 'kernel_origin=built_from_checkout\n' >> "$meta"
    if ! make -C "$REPO" all KERNEL_OUT="$kernel" > "$run/build.log" 2>&1; then
        die "内核构建失败，见 $run/build.log"
    fi
else
    printf 'kernel_origin=provided\n' >> "$meta"
fi
[ -f "$kernel" ] || die "找不到内核 $kernel"
kernel=$(cd "$(dirname "$kernel")" && pwd)/$(basename "$kernel")
kernel_hash=$(hash_file "$kernel")
printf 'kernel=%q\nkernel_sha256=%s\n' "$kernel" "$kernel_hash" >> "$meta"

truncate -s 64M "$scratch"
qemu_cmd=(
    timeout --kill-after=5 "$time_limit" "$qemu" -machine virt -m "$memory" -smp 1
    -nographic -bios default -kernel "$kernel" -no-reboot -snapshot
    -drive "file=${image//,/,,},if=none,format=raw,id=x0"
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
    -device virtio-net-device,netdev=net -netdev user,id=net -rtc base=utc
    -drive "file=${scratch//,/,,},if=none,format=raw,id=x1"
    -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
)
{
    printf 'qemu_command='
    printf '%q ' "${qemu_cmd[@]}"
    printf '\n'
} >> "$meta"

started=$SECONDS
qemu_exit=0
"${qemu_cmd[@]}" < /dev/null > "$serial" 2>&1 || qemu_exit=$?
printf 'qemu_exit=%s\nqemu_elapsed_seconds=%s\n' "$qemu_exit" "$((SECONDS - started))" >> "$meta"
if [ "$qemu_exit" -ne 0 ]; then
    printf 'QEMU 退出码 %s，见 %s\n' "$qemu_exit" "$serial" >&2
    exit "$qemu_exit"
fi

python3 - "$serial" "$basic" <<'PY'
from pathlib import Path
import sys

serial, output = map(Path, sys.argv[1:])
lines = serial.read_bytes().replace(b"\r", b"").replace(b"\0", b"").splitlines()
if any(b"[kernel] PANIC" in line or b"Panicked at" in line for line in lines):
    sys.exit(f"检测到内核 panic，见 {serial}")

start = [i for i, line in enumerate(lines) if line == b"#### OS COMP TEST GROUP START basic-musl ####"]
end = [i for i, line in enumerate(lines) if line == b"#### OS COMP TEST GROUP END basic-musl ####"]
if len(start) != 1 or len(end) != 1 or end[0] <= start[0]:
    sys.exit(f"basic-musl 缺少一对完整且有序的组标记，见 {serial}")

group = lines[start[0] + 1:end[0]]
if not any(line.startswith(b"========== START test_") for line in group):
    sys.exit(f"basic-musl 没有用例输出，见 {serial}")
if not any(line.startswith(b"[kernel] Idle process exit with exit_code 0 ") for line in lines[end[0] + 1:]):
    sys.exit(f"评测结束后未正常关机，见 {serial}")

output.write_bytes(b"\n".join(group) + b"\n")
PY

PYTHONDONTWRITEBYTECODE=1 SUITE="$SUITE" bash "$_ENV_DIR/score_basic.sh" "$basic" | tee "$run/score.txt"
