#!/usr/bin/env bash
# 构建 riscv64 musl 静态 lmbench_all（非 PIE，含 libtirpc 静态库）
#
# 关键点（踩过的坑）：
# 1. lmbench 源码里的 main 已经被改名为 xxx_main（oscomp 定制），
#    lmbench_all.c 用函数表逐个调用，因此必须直接构建 official 目标：
#    make OS=... O=../bin/<os> $O/lmbench_all
#    而不是 `make lmbench_all`（显式规则的目标名带 O 前缀，裸名字会走 built-in 规则，
#    链接时一个 .o 都没有 → undefined reference to `xxx_main`）。
# 2. libtirpc 的头文件在 libtirpc-1.3.6/tirpc/rpc/rpc.h，
#    因此 -I 要指向 libtirpc-1.3.6（以及其 tirpc 子目录），不是 include/。
# 3. musl 没有 sys/queue.h，从宿主 glibc 借一份。
set -euo pipefail

SUITE=${SUITE:-$HOME/testsuits}
BIN=${TOOLCHAIN:-$HOME/toolchains/riscv64-linux-musl-cross/bin}
CC="$BIN/riscv64-linux-musl-gcc"
SRC="$SUITE/lmbench_src"
WORK=${WORK:-$HOME/oscomp-work}
OUT=${1:-$WORK/lmbench}

STUB=/tmp/muslstub
mkdir -p "$STUB/sys"
[ -f "$STUB/sys/queue.h" ] || cp /usr/include/x86_64-linux-gnu/sys/queue.h "$STUB/sys/queue.h"

# ---- libtirpc（静态库，lmbench_all 需要）----
cd "$SRC"
if [ ! -f libtirpc-1.3.6/src/.libs/libtirpc.a ]; then
    rm -rf libtirpc-1.3.6
    tar zxf libtirpc-1.3.6.tar.gz
    cd libtirpc-1.3.6
    ./configure --host=riscv64-linux-musl --disable-shared --disable-gssapi \
        CC="$CC -static -no-pie" CPPFLAGS="-I$STUB" > /dev/null
    make -j"$(nproc)" > /tmp/libtirpc-build.log 2>&1
    cd ..
fi
echo "libtirpc: $(ls -l libtirpc-1.3.6/src/.libs/libtirpc.a | awk '{print $5}') bytes"

# ---- lmbench_all ----
O="$SRC/bin/riscv64-linux-musl"
mkdir -p "$O"
cd "$SRC/src"
make -k OS=riscv64-linux-musl O="$O" \
  CC="$CC -static -no-pie" \
  CFLAGS="-O2 -w -I$SRC/libtirpc-1.3.6 -I$SRC/libtirpc-1.3.6/tirpc -I$STUB" \
  "$O/lmbench_all" > /tmp/lmbench-build.log 2>&1 || true
if [ ! -x "$O/lmbench_all" ]; then
    echo "BUILD FAILED, tail of log:"; tail -20 /tmp/lmbench-build.log; exit 1
fi
echo "built lmbench_all: $(ls -l "$O/lmbench_all" | awk '{print $5}') bytes"

# ---- 本地模拟的 EXT4 测试盘（lmbench 组单独一张）----
mkdir -p "$OUT"
rm -f "$OUT/disk.img"
dd if=/dev/zero of="$OUT/disk.img" bs=1M count=64 status=none
mkfs.ext4 -q -F -L LMBENCH "$OUT/disk.img"

# hello：lat_proc shell 子测试要 exec 一个程序
cat > /tmp/lmb_hello.c <<'EOF'
int main(void) { return 0; }
EOF
"$CC" -static -no-pie -O1 -o "$OUT/hello" /tmp/lmb_hello.c

CMDS="$OUT/cmds.txt"
: > "$CMDS"
# 只缩短本地模拟盘的测量目标，评测原盘继续使用原始参数。
{
    printf 'export ENOUGH=10000\n'
    cat "$SUITE/scripts/lmbench/lmbench_testcode.sh"
} > "$OUT/lmbench_testcode.sh"
echo "write $OUT/lmbench_testcode.sh /lmbench_testcode.sh" >> "$CMDS"
echo "write $O/lmbench_all /lmbench_all" >> "$CMDS"
echo "write $SUITE/busybox/busybox /busybox" >> "$CMDS"
echo "write $OUT/hello /hello" >> "$CMDS"
echo "write $WORK/fsroot/target/oscomp_runner /oscomp_runner" >> "$CMDS"
echo "mkdir /var" >> "$CMDS"
echo "mkdir /var/tmp" >> "$CMDS"
echo "mkdir /tmp" >> "$CMDS"
debugfs -w -f "$CMDS" "$OUT/disk.img" > "$OUT/debugfs.log" 2>&1 || true
echo "image: $OUT/disk.img  ($(grep -c '^write' "$CMDS") files)"
