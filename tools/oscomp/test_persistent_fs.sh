#!/usr/bin/env bash
# Boot twice with the same writable EasyFS disk, then check invalid and read-only media.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

usage() {
    cat <<EOF
用法: $0 [riscv64|loongarch64|all] [日志目录]
默认测试两个架构。RISC-V 可以通过 QEMU_SMP=2 测试双核。
单架构可用 KERNEL、USER_BIN_DIR 和 QEMU 覆盖构建产物及模拟器。
可用 EASY_FS_FUSE 指定已构建的镜像打包工具。
EOF
}

case "${1:-all}" in
    -h|--help) usage; exit 0 ;;
    riscv64|loongarch64|all) arch=${1:-all} ;;
    *) usage; exit 1 ;;
esac
[ "$#" -le 2 ] || die "参数过多"
logs=${2:-$WORK/persistent-fs}
case "${QEMU_SMP:-1}" in
    1|2) ;;
    *) die "仅支持 1 或 2 核" ;;
esac
if [ "$arch" = loongarch64 ] && [ "${QEMU_SMP:-1}" != 1 ]; then
    die "LoongArch 仅支持单核"
fi
if [ "$arch" = all ] && [ -n "${KERNEL:-}${USER_BIN_DIR:-}${QEMU:-}" ]; then
    die "使用 KERNEL、USER_BIN_DIR 或 QEMU 时需要指定单个架构"
fi

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$logs" "$scratch/empty"
formatter=${EASY_FS_FUSE:-$scratch/formatter/release/easy-fs-fuse}
if [ -z "${EASY_FS_FUSE:-}" ]; then
    (cd "$REPO/easy-fs-fuse" && cargo +stable build --locked --release --target-dir "$scratch/formatter")
fi
"$formatter" -s "$scratch/empty/" -t "$scratch/"

run_boot() {
    local phase=$1 marker=$2 data=$3 disk_mode=$4
    local log=$logs/$arch-$phase.log
    printf '%.1s' "$phase" > "$run_dir/root/persist-phase"
    truncate -s 32M "$run_dir/$phase.img"
    mkfs.ext4 -q -F -b 4096 -d "$run_dir/root" "$run_dir/$phase.img"
    sha256sum "$run_dir/$phase.img" > "$run_dir/$phase.sha256"

    timeout --kill-after=5 "${QEMU_TIMEOUT:-45}" "${QEMU:-qemu-system-$arch}" \
        -machine virt -smp "$harts" -m "$memory" -nographic -monitor none -nic none \
        -kernel "$kernel" -no-reboot \
        -drive "file=$run_dir/$phase.img,if=none,format=raw,id=x0,readonly=on" \
        -drive "file=$data,if=none,format=raw,id=x1,$disk_mode" \
        "${platform[@]}" < /dev/null > "$log" 2>&1 || die "QEMU 未正常退出，见 $log"

    sha256sum --status --check "$run_dir/$phase.sha256" || die "EXT4 测试盘内容发生变化"
    if grep -aqE '\[kernel\] PANIC|Panicked at|Panicked:' "$log" ||
       ! grep -aqF "[persist-abi] $marker PASS" "$log" ||
       ! grep -aqF 'Idle process exit with exit_code 0' "$log"; then
        die "$arch EasyFS $marker 回归失败，见 $log"
    fi
}

run_arch() (
    local arch=$1 harts=$2
    local run_dir=$scratch/$arch target bins kernel memory smp
    local -a platform user_build
    mkdir -p "$run_dir/root/persist" "$run_dir/root/other"
    kernel=${KERNEL:-$run_dir/kernel}
    case "$arch" in
        riscv64)
            target=riscv64gc-unknown-none-elf
            user_build=(cargo build)
            memory=256M
            platform=(-bios default
                -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
                -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1)
            if [ -z "${KERNEL:-}" ]; then
                smp=$((harts - 1))
                make -C "$REPO" all SMP="$smp" KERNEL_OUT="$kernel"
            fi
            ;;
        loongarch64)
            target=loongarch64-unknown-none-softfloat
            user_build=(env RUSTC_BOOTSTRAP=user_lib cargo "+${LA_TOOLCHAIN:-1.98.1}" build)
            memory=1G
            platform=(-device virtio-blk-pci,drive=x0,disable-legacy=on,romfile=
                -device virtio-blk-pci,drive=x1,disable-legacy=on,romfile=)
            if [ -z "${KERNEL:-}" ]; then
                make -C "$REPO" kernel-la LA_KERNEL_OUT="$kernel"
            fi
            ;;
    esac
    if [ -z "${USER_BIN_DIR:-}" ]; then
        (cd "$REPO/user" && "${user_build[@]}" --locked --release --target "$target" \
            --bin persist_abi_test --bin oscomp_runner)
    fi
    bins=${USER_BIN_DIR:-${CARGO_TARGET_DIR:-$REPO/user/target}/$target/release}
    cp "$bins/persist_abi_test" "$bins/oscomp_runner" "$run_dir/root/"
    printf './persist_abi_test\n' > "$run_dir/root/basic_testcode.sh"
    printf 'lower' > "$run_dir/root/persist/lower"
    local data=$run_dir/data.img blank=$run_dir/blank-data.img readonly=$run_dir/readonly-data.img
    cp "$scratch/fs.img" "$data"
    run_boot write WRITE "$data" readonly=off
    run_boot verify VERIFY "$data" readonly=off

    truncate -s "$(wc -c < "$scratch/fs.img" | tr -d '[:space:]')" "$blank"
    cp "$scratch/fs.img" "$readonly"
    sha256sum "$blank" "$readonly" > "$run_dir/rejected.sha256"
    run_boot blank BLANK "$blank" readonly=off
    run_boot readonly READONLY "$readonly" readonly=on
    sha256sum --status --check "$run_dir/rejected.sha256" || die "测试中的空白盘或只读盘内容发生变化"
    printf '%s EasyFS 回归通过，日志目录 %s\n' "$arch" "$logs"
)

if [ "$arch" = all ]; then
    run_arch riscv64 "${QEMU_SMP:-1}"
    run_arch loongarch64 1
else
    run_arch "$arch" "${QEMU_SMP:-1}"
fi
