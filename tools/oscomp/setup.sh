#!/usr/bin/env bash
# setup.sh — 一次性准备评测测试盘所需的外部依赖（幂等，可重复运行）
#
#   1. 官方测试套件源码  -> $SUITE      （oscomp/testsuits-for-oskernel，分支 2025_multiarch）
#   2. riscv64 musl 交叉编译器 -> $TOOLCHAIN （musl.cc）
#   3. cmake（预编译包）  -> $CMAKE_BIN  （basic 组用 CMake 构建）
#   4. 检查宿主机需要的包，缺什么就打印对应的 apt 命令（本脚本不 sudo）
#
# 踩坑记录：
#   - 分支必须是 2025_multiarch：它的目录（basic/ busybox/ libc-test/ ltp-full-20240524/
#     lmbench_src/ scripts/<组>/ ...）与仓库适配的 12 个评测组一一对应。
#     main 是 2021 年的旧版；final-2025 / final-2026 是另外的赛题，没有这些目录。
#   - GitHub 对该分支的 clone 经常报 "invalid index-pack output"，gitee 镜像可用，故优先 gitee。
#   - musl.cc 经代理下载常在中途 EOF 断开，所以用 curl -C - 断点续传循环。
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

BRANCH=${BRANCH:-2025_multiarch}
GIT_URLS=(https://gitee.com/oscomp/testsuits-for-oskernel.git
          https://github.com/oscomp/testsuits-for-oskernel.git)
MUSL_URL=${MUSL_URL:-https://musl.cc/riscv64-linux-musl-cross.tgz}
CMAKE_VER=${CMAKE_VER:-3.31.6}
CMAKE_URL=https://github.com/Kitware/CMake/releases/download/v$CMAKE_VER/cmake-$CMAKE_VER-linux-x86_64.tar.gz

# fetch <url> <输出文件>：断点续传，直到 gzip 校验通过（最多 40 轮）
fetch() {
    local url=$1 out=$2 i
    for i in $(seq 1 40); do
        curl -sS -fL -C - --connect-timeout 20 --max-time 120 -o "$out" "$url" 2>/dev/null || true
        if [ -f "$out" ] && gzip -t "$out" 2>/dev/null; then return 0; fi
        sleep 2
    done
    die "下载失败：$url"
}

echo "== [1/4] 测试套件源码 -> $SUITE"
if [ -d "$SUITE/scripts/basic" ]; then
    echo "   已存在，跳过（分支: $(git -C "$SUITE" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')）"
else
    rm -rf "$SUITE"
    ok=0
    for url in "${GIT_URLS[@]}"; do
        for try in 1 2 3; do
            if git clone -q --depth 1 -b "$BRANCH" "$url" "$SUITE" 2>/dev/null && [ -d "$SUITE/.git" ]; then
                ok=1; break 2
            fi
            rm -rf "$SUITE"; sleep 3
        done
    done
    [ "$ok" = 1 ] || die "clone 失败（分支 $BRANCH）"
    echo "   OK: $(git -C "$SUITE" log -1 --format='%h %ad %s' --date=short)"
fi

echo "== [2/4] musl 交叉编译器 -> $TOOLCHAIN"
if [ -x "$CC" ]; then
    echo "   已存在: $("$CC" --version | head -1)"
else
    mkdir -p "$HOME/toolchains"
    tgz="$HOME/toolchains/riscv64-linux-musl-cross.tgz"
    fetch "$MUSL_URL" "$tgz"
    tar -C "$HOME/toolchains" -xzf "$tgz" && rm -f "$tgz"
    echo "   OK: $("$CC" --version | head -1)"
fi

echo "== [3/4] cmake -> $CMAKE_BIN"
if [ -x "$CMAKE_BIN/cmake" ]; then
    echo "   已存在: $("$CMAKE_BIN/cmake" --version | head -1)"
else
    mkdir -p "$HOME/toolchains"
    tgz="$HOME/toolchains/cmake-$CMAKE_VER.tar.gz"
    fetch "$CMAKE_URL" "$tgz"
    tar -C "$HOME/toolchains" -xzf "$tgz" && rm -f "$tgz"
    echo "   OK: $("$CMAKE_BIN/cmake" --version | head -1)"
fi

echo "== [4/4] 宿主机工具检查"
missing=()
for c in gcc make git curl tar gzip xz mkfs.ext4 e2fsck qemu-system-riscv64 cargo; do
    command -v "$c" >/dev/null 2>&1 || missing+=("$c")
done
[ ${#missing[@]} -eq 0 ] && echo "   基础工具齐全" || echo "   缺少（必需）: ${missing[*]}"
opt=()
for c in autoconf automake m4 pkg-config bison flex libtool; do
    command -v "$c" >/dev/null 2>&1 || opt+=("$c")
done
if [ ${#opt[@]} -gt 0 ]; then
    echo "   缺少（仅 ltp / iperf / netperf 需要）: ${opt[*]}"
    echo "   需要时执行: sudo apt install -y autoconf automake m4 pkg-config bison flex libtool"
fi
echo "== setup 完成"
