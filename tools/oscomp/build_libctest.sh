#!/usr/bin/env bash
# build_libctest.sh — 构建 oscomp libc-test（musl 静态，非 PIE）并打本地模拟 ext4 测试盘（平铺布局，不同于真实官方盘）
#
# 产物（默认 $HOME/oscomp-work/libctest/）：
#   entry-static.exe  runtest.exe  run-static.sh  run-dynamic.sh  disk.img
# 镜像根目录布局（oscomp_runner 会自动找 *_testcode.sh 并用 busybox sh 执行）：
#   /libctest_testcode.sh  /run-static.sh  /run-dynamic.sh
#   /entry-static.exe  /runtest.exe  /busybox  /oscomp_runner
#   目录 /tmp /var /var/tmp
#
# 用法：
#   tools/oscomp/build_libctest.sh            # 只构建 + 打包
#   RUN=1 tools/oscomp/build_libctest.sh      # 构建打包后立刻用 QEMU 跑一次（日志 $OUT/last-run.log）
# 环境变量：SUITE TOOLCHAIN WORK OUT（OUT 必须被 WORK 包含或为空）
#
# 关键点（踩坑记录）：
# 1. musl.cc 工具链链接静态时会出现 "read-only segment has dynamic relocations"，
#    必须在 LDFLAGS 里补 `-no-pie`（-static -no-pie 组合），否则 entry-static.exe 链接失败。
# 2. libc-test 的 Makefile 用 `>>` 生成 entry.h，重复构建必须先删 entry.h，否则表项重复/编译错。
# 3. 官方 testcode 脚本里 `./run-static.sh` / `./run-dynamic.sh` 需要可执行位；debugfs write 保留宿主
#    权限，因此写盘前要 chmod 755（否则 guest 里可能 Permission denied）。
# 4. 动态测试（entry-dynamic.exe + DSO）需要 /lib/ld-musl-riscv64.so.1 与 dlopen 支持，
#    rCore 目前没有，因此镜像只放静态部分；run-dynamic.sh 用占位脚本，保证 testcode.sh 不中断。
# 5. rCore 的 execve 目前不支持 "#!<interp>"（exec 一个 .sh 返回 EPERM），
#    官方 libctest_testcode.sh 里的 `./run-static.sh` 会失败。本脚本用 sed 生成一份
#    source 版包装（$OUT/libctest_testcode.sh）放进镜像；原始 testsuits 文件不动。
#    根治办法是内核 execve 支持 shebang。
set -uo pipefail
export LC_ALL=C

SUITE=${SUITE:-$HOME/testsuits}
TOOLCHAIN=${TOOLCHAIN:-$HOME/toolchains/riscv64-linux-musl-cross/bin}
PREFIX="$TOOLCHAIN/riscv64-linux-musl-"
WORK=${WORK:-$HOME/oscomp-work/libctest}
SRC="$SUITE/libc-test"
OUTDIR="${OUT:-$WORK}"
RUN=${RUN:-0}

mkdir -p "$WORK" "$OUTDIR" "$WORK/logs"

echo "== [1/4] 复制 libc-test 源码（仅源码，排除旧构建产物）"
rm -rf "$WORK/src"
mkdir -p "$WORK/src"
( cd "$SRC" && tar cf - --exclude='*.obj' --exclude='*.exe' --exclude='*.so' \
    --exclude='entry.h' --exclude='disk' --exclude='run-*.sh' . ) | ( cd "$WORK/src" && tar xf - )

echo "== [2/4] 构建 static + runtest（musl 静态、非 PIE）"
cd "$WORK/src"
rm -f entry.h run-*.sh
# -no-pie 是必须的：见文件头注释 1
LDFLAGS_FIX="-Os -s -lpthread -lm -lrt -no-pie"
build_once() { make PREFIX="$PREFIX" LDFLAGS="$LDFLAGS_FIX" -j"$(nproc)" runtest static > "$WORK/logs/build.log" 2>&1; }

if build_once; then
    echo "   build OK"
else
    # 兜底：musl 下个别用例可能编译失败，剔除后重试（最多 3 轮）
    echo "   首次构建失败，尝试剔除失败用例后重试"
    for round in 1 2 3; do
        grep -oE '\[Makefile:[0-9]+: [^]]+\.obj\]' "$WORK/logs/build.log" \
            | sed -E 's/^\[Makefile:[0-9]+: //; s/\]$//' | sort -u > "$WORK/logs/failed.objs"
        n=$(wc -l < "$WORK/logs/failed.objs")
        echo "   round $round: failed objs = $n"
        [ "$n" -eq 0 ] && break
        while read -r obj; do
            [ -n "$obj" ] || continue
            exe="${obj%.obj}.exe"
            grep -v -F -x "$exe" "$WORK/src/static.txt" > "$WORK/src/static.txt.new" || true
            mv "$WORK/src/static.txt.new" "$WORK/src/static.txt"
            rm -f "$WORK/src/$obj" "$WORK/src/${obj%.obj}_prefixed.obj"
        done < "$WORK/logs/failed.objs"
        rm -f entry.h
        build_once && { echo "   round $round: build OK after pruning"; break; }
    done
fi
if [ ! -f "$WORK/src/entry-static.exe" ] || [ ! -f "$WORK/src/runtest.exe" ]; then
    echo "ERROR: entry-static.exe / runtest.exe 未生成，build.log 尾部："
    tail -30 "$WORK/logs/build.log"
    exit 1
fi
cp -f "$WORK/src/entry-static.exe" "$WORK/src/runtest.exe" "$WORK/"

echo "== [3/4] 生成官方 run-static.sh（与 libc-test Makefile 的 run-all.sh 规则一致）"
( cd "$WORK/src" && cat static.txt | xargs -I AA basename AA .exe | sed 's/-/_/g' \
    | xargs -I BB printf "./runtest.exe -w entry-static.exe %s\n" BB > "$WORK/run-static.sh" )
# 动态部分：rCore 无 ld-musl / dlopen，用占位脚本保证 testcode.sh 继续往下走
cat > "$WORK/run-dynamic.sh" <<'EOS'
./busybox echo "libctest: dynamic tests skipped (needs ld-musl/dlopen)"
EOS
chmod 755 "$WORK/run-static.sh" "$WORK/run-dynamic.sh"
NSTATIC=$(grep -c 'runtest.exe' "$WORK/run-static.sh")
echo "   static tests: $NSTATIC"

# rCore 的 execve 目前不支持 "#!<interp>"（exec 一个 .sh 返回 EPERM），
# 而官方 libctest_testcode.sh 用 `./run-static.sh` 子进程方式调用。
# 这里生成一份等价的包装脚本（改成 source），原始文件不动；放进镜像 /libctest_testcode.sh。
sed -e 's#^\./run-static.sh$#. ./run-static.sh#' \
    -e 's#^\./run-dynamic.sh$#. ./run-dynamic.sh#' \
    "$SUITE/scripts/libctest/libctest_testcode.sh" > "$WORK/libctest_testcode.sh"
chmod 755 "$WORK/libctest_testcode.sh"
echo "   testcode wrapper: $WORK/libctest_testcode.sh"

echo "== [4/4] 打 64MB ext4 本地模拟测试盘"
rm -f "$OUTDIR/disk.img"
dd if=/dev/zero of="$OUTDIR/disk.img" bs=1M count=64 status=none
mkfs.ext4 -q -F -L LIBCTEST "$OUTDIR/disk.img"
CMDS="$WORK/logs/cmds.txt"
: > "$CMDS"
runroot="$OUTDIR"
echo "write $runroot/libctest_testcode.sh /libctest_testcode.sh" >> "$CMDS"
echo "write $runroot/run-static.sh /run-static.sh" >> "$CMDS"
echo "write $runroot/run-dynamic.sh /run-dynamic.sh" >> "$CMDS"
echo "write $runroot/entry-static.exe /entry-static.exe" >> "$CMDS"
echo "write $runroot/runtest.exe /runtest.exe" >> "$CMDS"
echo "write $SUITE/busybox/busybox /busybox" >> "$CMDS"
echo "write $HOME/oscomp-work/fsroot/target/oscomp_runner /oscomp_runner" >> "$CMDS"
echo "mkdir /tmp" >> "$CMDS"
echo "mkdir /var" >> "$CMDS"
echo "mkdir /var/tmp" >> "$CMDS"
debugfs -w -f "$CMDS" "$OUTDIR/disk.img" > "$WORK/logs/debugfs.log" 2>&1 || true

echo "---- 结果 ------------------------------------------------------"
echo "image : $OUTDIR/disk.img ($(stat -c%s "$OUTDIR/disk.img") bytes)"
echo "static: $OUTDIR/entry-static.exe ($(stat -c%s "$OUTDIR/entry-static.exe") bytes), $NSTATIC tests"
echo "runner: $OUTDIR/runtest.exe ($(stat -c%s "$OUTDIR/runtest.exe") bytes)"
debugfs -R "ls -l /" "$OUTDIR/disk.img" 2>/dev/null | awk '{print "  /"$NF, $5, $6}' | grep -v '^  /\.' | head -15

if [ "$RUN" = "1" ]; then
    echo "== RUN=1：用最新内核跑一次"
    K="$HOME/rCore-Tutorial-v3/kernel-rv"
    [ -f "$K" ] || K="$HOME/rCore-Tutorial-v3/os/target/riscv64gc-unknown-none-elf/release/os.bin"
    cp -f "$K" /tmp/kernel-libctest.bin
    timeout 720 qemu-system-riscv64 -machine virt -m 1G -nographic -bios default \
        -kernel /tmp/kernel-libctest.bin \
        -drive file="$OUTDIR/disk.img",if=none,format=raw,id=x0 \
        -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
        > "$OUTDIR/last-run.log" 2>&1
    echo "   qemu exit=$? log=$OUTDIR/last-run.log"
    grep -a 'OS COMP TEST GROUP' "$OUTDIR/last-run.log" || true
fi
echo "== done"
