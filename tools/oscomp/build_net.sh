#!/usr/bin/env bash
# 构建 riscv64 musl 静态 iperf3 / netperf 并打本地模拟的组盘。
#
# 说明：rCore-QEMU 侧没有 TCP/IP 栈（socket/connect/bind 等 syscall 未实现），
# 这两组的子测试必然 fail；本脚本的目的是让官方 *_testcode.sh **能跑完并打
# START/END 标记 + 自动关机**（即“组能完成”，便于对照评测机的输出格式），
# 真正的得分需要网卡驱动 + 网络协议栈。
#
# 踩坑：
# - iperf3 必须 `--without-openssl`，否则静态链接时找不到 -lssl/-lcrypto；
# - 两者都要 `-static -no-pie`（rCore 不支持动态链接/PIE 重定位）。
set -uo pipefail

SUITE=${SUITE:-$HOME/testsuits}
BIN=${TOOLCHAIN:-$HOME/toolchains/riscv64-linux-musl-cross/bin}
CC="$BIN/riscv64-linux-musl-gcc"
WORK=${WORK:-$HOME/oscomp-work}

# ---------------- iperf3 ----------------
cd "$SUITE/iperf"
make -s clean >/dev/null 2>&1 || true
./configure --host=riscv64-linux-musl CC="$CC" \
    --enable-static-bin --disable-shared --without-openssl \
    CFLAGS="-O2 -w -static -no-pie" LDFLAGS="-static -no-pie" \
    > /tmp/iperf-configure.log 2>&1
make -j"$(nproc)" > /tmp/iperf-make.log 2>&1
echo "iperf3: $(ls -l src/iperf3 | awk '{print $5}') bytes"

OUT="$WORK/iperf"
mkdir -p "$OUT"
rm -f "$OUT/disk.img"
dd if=/dev/zero of="$OUT/disk.img" bs=1M count=64 status=none
mkfs.ext4 -q -F -L IPERF "$OUT/disk.img"
CMDS="$OUT/cmds.txt"; : > "$CMDS"
echo "write $SUITE/scripts/iperf/iperf_testcode.sh /iperf_testcode.sh" >> "$CMDS"
echo "write $SUITE/iperf/src/iperf3 /iperf3" >> "$CMDS"
echo "write $SUITE/busybox/busybox /busybox" >> "$CMDS"
echo "write $WORK/fsroot/target/oscomp_runner /oscomp_runner" >> "$CMDS"
echo "mkdir /tmp" >> "$CMDS"
echo "mkdir /var" >> "$CMDS"
echo "mkdir /var/tmp" >> "$CMDS"
debugfs -w -f "$CMDS" "$OUT/disk.img" > "$OUT/debugfs.log" 2>&1 || true
echo "image: $OUT/disk.img"

# ---------------- netperf ----------------
cd "$SUITE/netperf"
[ -d netperf-2.7.0 ] || tar zxf netperf-2.7.0.tar.gz
cd netperf-2.7.0
./configure --build="$(gcc -dumpmachine)" --host=riscv64-linux-musl \
    CC="$CC -static -no-pie" CFLAGS="-O2 -w" LDFLAGS="-static -no-pie" \
    > /tmp/netperf-configure.log 2>&1
make -j"$(nproc)" > /tmp/netperf-make.log 2>&1
echo "netperf: $(ls -l src/netperf | awk '{print $5}') bytes, netserver: $(ls -l src/netserver | awk '{print $5}') bytes"

OUT="$WORK/netperf"
mkdir -p "$OUT"
rm -f "$OUT/disk.img"
dd if=/dev/zero of="$OUT/disk.img" bs=1M count=64 status=none
mkfs.ext4 -q -F -L NETPERF "$OUT/disk.img"
CMDS="$OUT/cmds.txt"; : > "$CMDS"
echo "write $SUITE/scripts/netperf/netperf_testcode.sh /netperf_testcode.sh" >> "$CMDS"
echo "write $SUITE/netperf/netperf-2.7.0/src/netperf /netperf" >> "$CMDS"
echo "write $SUITE/netperf/netperf-2.7.0/src/netserver /netserver" >> "$CMDS"
echo "write $SUITE/busybox/busybox /busybox" >> "$CMDS"
echo "write $WORK/fsroot/target/oscomp_runner /oscomp_runner" >> "$CMDS"
echo "mkdir /tmp" >> "$CMDS"
echo "mkdir /var" >> "$CMDS"
echo "mkdir /var/tmp" >> "$CMDS"
debugfs -w -f "$CMDS" "$OUT/disk.img" > "$OUT/debugfs.log" 2>&1 || true
echo "image: $OUT/disk.img"
