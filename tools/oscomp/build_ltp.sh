#!/usr/bin/env bash
# =============================================================================
# build_ltp.sh -- 交叉构建 LTP（20240524）riscv64 musl 静态子集并在 rCore 上实跑
#
# 背景：rCore-Tutorial-v3 的 oscomp 适配需要 LTP 测试盘。LTP 官方入口
# build-oscomp.sh 需要宿主 autoconf/automake/m4，但评测/部署镜像里往往没有
# 这些工具、也没有 root。本脚本把新版 autotools 通过 `apt-get download` 下载
# 成 .deb（无需 root）并解包到私有前缀，改写成可重定位路径后完成：
#   autotools -> configure(cross) -> 构建 lib + syscall 子集 -> 打 128MB ext4
#   本地模拟测试盘 -> （可选）QEMU 实跑。
#
# 用法：
#   bash tools/oscomp/build_ltp.sh            # 构建 + 打盘，不跑 QEMU
#   RUN=1 bash tools/oscomp/build_ltp.sh      # 构建 + 打盘 + QEMU 实跑
#   FULL=1 bash tools/oscomp/build_ltp.sh     # 构建全部 testcases（很慢，10min+）
#   OUT=$HOME/oscomp-work/ltp RUN=1 ...       # 自定义输出目录
#
# 输出：
#   $OUT/ltp-disk.img         128MB ext4 测试盘（根目录 /oscomp_runner + /ltp_testcode.sh
#                             + /busybox + /ltp/testcases/bin/*）
#   $OUT/first-run.log        QEMU 串口日志（RUN=1 时）
#   $OUT/logs/                构建日志
#
# 关键点 / 踩坑记录：
#   1. LTP autotools 需要 autoconf/automake/m4；宿主没有且无 sudo 时用
#      apt-get download + dpkg -x 解到私有前缀，再把脚本里硬编码的
#      /usr/share、/usr/bin/<tool> 改成本前缀（autom4te.cfg 的
#      --prepend-include、Automake/Config.pm 的 $libdir 都必须改）。
#   2. config.guess/config.sub 在 Debian 的 automake 包里是指向
#      /usr/share/misc/ 的符号链接，需额外 apt-get download autotools-dev。
#   3. musl 交叉 gcc 默认产出 PIE 动态链接，必须 -static -no-pie，
#      否则 rCore 无法 exec。
#   4. 测试二进制带 debug_info 很大，打包前 strip。
#   5. LTP 官方 ltp_testcode.sh 用 `basename`，rCore 根文件系统只有
#      /busybox，PATH 里没有 basename；本地版脚本改用 shell 参数展开。
#   6. tst_tmpdir 会 chown，需要内核实现 fchownat(54)/faccessat(48)，
#      否则绝大多数用例一启动就 TBROK（见 logs 里的 unsupported syscall）。
# =============================================================================
set -euo pipefail

SUITE=${SUITE:-$HOME/testsuits}
SRC=${SRC:-$SUITE/ltp-full-20240524}
OUT=${OUT:-$HOME/oscomp-work/ltp}
TOOLCHAIN=${TOOLCHAIN:-$HOME/toolchains/riscv64-linux-musl-cross/bin}
KERNEL=${KERNEL:-$HOME/rCore-Tutorial-v3/kernel-rv}
[ -f "$KERNEL" ] || KERNEL=$HOME/rCore-Tutorial-v3/os/target/riscv64gc-unknown-none-elf/release/os.bin
WORK=$OUT/imgwork
LOGD=$OUT/logs
AT=$OUT/autotools
export PATH="$AT/usr/bin:$TOOLCHAIN:$PATH"
export ACLOCAL_PATH=/usr/share/aclocal
CC_BIN=$TOOLCHAIN/riscv64-linux-musl-gcc
CC_CMD="$CC_BIN -static -no-pie"
mkdir -p "$OUT" "$LOGD" "$WORK/ltp/testcases/bin"

# 只构建这批 syscall 目录（想扩子集就改这里；FULL=1 时忽略）
DIRS=${DIRS:-"getpid getppid getuid getgid geteuid getegid uname gettimeofday times \
close dup dup2 write read open openat lseek access chdir getcwd mkdir rmdir unlink \
pipe alarm pause brk truncate ftruncate fstat stat fsync chmod chown"}

# ---------------------------------------------------------------- autotools
setup_autotools() {
    if [ -f "$AT/.relocated" ] && "$AT/usr/bin/aclocal" --version >/dev/null 2>&1; then
        echo "== [1/5] autotools 已就绪: $AT"
        return 0
    fi
    echo "== [1/5] 安装私有 autotools 前缀 -> $AT"
    mkdir -p "$OUT/debs" "$AT"
    ( cd "$OUT/debs"
      ls m4_*.deb autoconf_*.deb automake_*.deb autotools-dev_*.deb >/dev/null 2>&1 || \
        apt-get download m4 autoconf automake autotools-dev )
    for d in "$OUT/debs"/*.deb; do dpkg -x "$d" "$AT"; done
    ( cd "$AT/usr/bin"
      ln -sf aclocal-1.16 aclocal
      ln -sf automake-1.16 automake
      sed -i "s#/usr/share#$AT/usr/share#g" \
          aclocal-1.16 automake-1.16 autoconf autoheader autom4te autoreconf autoscan autoupdate ifnames
      sed -i "s#/usr/bin/autom4te#${AT}/usr/bin/autom4te#g; s#/usr/bin/autoheader#${AT}/usr/bin/autoheader#g; \
              s#/usr/bin/autoconf#${AT}/usr/bin/autoconf#g; s#/usr/bin/autoreconf#${AT}/usr/bin/autoreconf#g; \
              s#/usr/bin/automake#${AT}/usr/bin/automake#g; s#/usr/bin/aclocal#${AT}/usr/bin/aclocal#g; \
              s#/usr/bin/m4#${AT}/usr/bin/m4#g" autom4te autoconf autoheader autoreconf autoscan autoupdate
      # 若脚本对已 patch 过的前缀重跑会出现双前缀，统一回滚
      for f in aclocal-1.16 automake-1.16 autom4te autoconf autoheader autoreconf autoscan autoupdate ifnames; do
          sed -i "s#${AT}${AT}#${AT}#g" "$f"
      done
    )
    sed -i "s#/usr/share/autoconf#${AT}/usr/share/autoconf#g" \
        "$AT/usr/share/autoconf/autom4te.cfg"
    sed -i "s#/usr/share#${AT}/usr/share#g" \
        "$AT/usr/share/automake-1.16/Automake/Config.pm"
    # Debian automake 的 config.guess/sub 是指向 ../misc/ 的符号链接（autotools-dev 包）
    touch "$AT/.relocated"
    echo "   aclocal/automake/autoconf 就绪"
}

# ---------------------------------------------------------- 源码 oscomp 适配
patch_source() {
    # 这些修改是 oscomp 官方 build-oscomp.sh 注释里声明的（本仓库源码已应用，
    # 若换官方 ltp-full tarball 则需在此应用，重复执行无副作用）。
    sed -i 's|#include <sys/sysinfo.h>|#include <linux/sysinfo.h>|g' \
        "$SRC"/testcases/kernel/syscalls/*/*.c 2>/dev/null || true
    local mk=$SRC/testcases/kernel/syscalls/Makefile
    if ! grep -q 'FILTER_OUT_DIRS += fmtmsg' "$mk" 2>/dev/null; then
        echo 'FILTER_OUT_DIRS += fmtmsg rt_sigtimedwait rt_tgsigqueueinfo timer_create' >> "$mk"
    fi
}

# ---------------------------------------------------------------- 构建
build_ltp() {
    echo "== [2/5] make autotools"
    patch_source
    ( cd "$SRC" && make autotools > "$LOGD/autotools.log" 2>&1 )
    echo "== [3/5] configure (cross riscv64 musl)"
    ( cd "$SRC" && ./configure \
        --prefix=/riscv64-linux-musl/ltp \
        --host=riscv64-linux-musl \
        CC="$CC_CMD" \
        --with-target-cflags='-march=rv64gc' \
        --without-tirpc > "$LOGD/configure.log" 2>&1 )
    echo "== [4/5] 构建 lib + 测试用例"
    ( cd "$SRC" && make -C lib -j"$(nproc)" > "$LOGD/lib.log" 2>&1 )
    [ -f "$SRC/lib/libltp.a" ] || { echo "!! libltp.a 未生成"; exit 1; }
    if [ "${FULL:-0}" = "1" ]; then
        ( cd "$SRC" && make -j"$(nproc)" > "$LOGD/all.log" 2>&1 )
    else
        for d in $DIRS; do
            [ -d "$SRC/testcases/kernel/syscalls/$d" ] || continue
            echo "$d"
        done | xargs -P "$(nproc)" -I{} bash -c \
            "make -C '$SRC/testcases/kernel/syscalls/{}' -j2 > '$LOGD/{}.log' 2>&1 || echo 'FAIL {}'"
    fi
}

# ---------------------------------------------------------------- 打包
pack_image() {
    echo "== [5/5] 收集二进制 + 打 128MB ext4 盘"
    rm -rf "$WORK/ltp/testcases/bin"; mkdir -p "$WORK/ltp/testcases/bin"
    : > "$OUT/binlist.txt"
    if [ "${FULL:-0}" = "1" ]; then
        # 全量：按 install 布局从各源码目录收集可执行文件
        find "$SRC/testcases" -maxdepth 4 -type f -perm -u+x \
            -name '*' ! -name '*.c' ! -name '*.h' ! -name '*.o' > "$OUT/allbins.tmp" 2>/dev/null || true
        while read -r f; do cp -f "$f" "$WORK/ltp/testcases/bin/${f##*/}"; done < "$OUT/allbins.tmp"
    else
        for d in $DIRS; do
            [ -d "$SRC/testcases/kernel/syscalls/$d" ] || continue
            for f in "$SRC/testcases/kernel/syscalls/$d"/*; do
                [ -f "$f" ] && [ -x "$f" ] || continue
                case "$f" in *.c|*.h|*.o|*.d|*.mk|Makefile*) continue;; esac
                cp -f "$f" "$WORK/ltp/testcases/bin/${f##*/}"
            done
        done
    fi
    "$TOOLCHAIN/riscv64-linux-musl-strip" "$WORK/ltp/testcases/bin/"* 2>/dev/null || true
    echo "   用例数: $(ls "$WORK/ltp/testcases/bin" | wc -l), 大小: $(du -sh "$WORK/ltp/testcases/bin" | cut -f1)"

    # 本地 testcode：保留官方 START/END 标记，去掉 basename 依赖，加单例超时
    cat > "$WORK/ltp_testcode.sh" <<'EOS'
#!/bin/bash
cd /
export PATH=/:/bin:/sbin:/usr/bin:/usr/sbin
echo "#### OS COMP TEST GROUP START ltp ####"
target_dir="ltp/testcases/bin"
for file in "$target_dir"/*; do
  if [ -f "$file" ]; then
    name="${file##*/}"
    echo "RUN LTP CASE $name"
    /busybox timeout 30 "$file"
    ret=$?
    echo "FAIL LTP CASE $name : $ret"
  fi
done
echo "#### OS COMP TEST GROUP END ltp ####"
EOS
    chmod 755 "$WORK/ltp_testcode.sh"

    local img=$OUT/ltp-disk.img cmds=$LOGD/cmds.txt
    rm -f "$img"
    dd if=/dev/zero of="$img" bs=1M count=128 status=none
    mkfs.ext4 -q -F -L LTP "$img"
    : > "$cmds"
    echo "write $WORK/ltp_testcode.sh /ltp_testcode.sh"          >> "$cmds"
    echo "write $SUITE/busybox/busybox /busybox"                 >> "$cmds"
    echo "write $HOME/oscomp-work/fsroot/target/oscomp_runner /oscomp_runner" >> "$cmds"
    for d in ltp ltp/testcases ltp/testcases/bin tmp var var/tmp dev; do echo "mkdir /$d" >> "$cmds"; done
    for f in "$WORK/ltp/testcases/bin/"*; do
        echo "write $f /ltp/testcases/bin/${f##*/}" >> "$cmds"
    done
    debugfs -w -f "$cmds" "$img" > "$LOGD/debugfs.log" 2>&1 || true
    echo "   镜像: $img ($(stat -c%s "$img") bytes)"
}

# ---------------------------------------------------------------- 实跑
run_qemu() {
    [ "${RUN:-0}" = "1" ] || { echo "== RUN!=1，跳过 QEMU 实跑"; return 0; }
    echo "== QEMU 实跑（timeout 480s）"
    cp -f "$KERNEL" /tmp/kernel-ltp.bin
    timeout 480 qemu-system-riscv64 -machine virt -m 1G -nographic -bios default \
        -kernel /tmp/kernel-ltp.bin \
        -drive file="$OUT/ltp-disk.img",if=none,format=raw,id=x0 \
        -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
        > "$OUT/first-run.log" 2>&1 || true
    echo "   qemu 结束, log=$OUT/first-run.log"
    grep -a 'OS COMP TEST GROUP' "$OUT/first-run.log" || echo "   !! 未出现 START/END 标记"
    echo "   RUN 用例数: $(grep -ac 'RUN LTP CASE' "$OUT/first-run.log" || true)"
}

setup_autotools
build_ltp
pack_image
run_qemu
echo "== done: OUT=$OUT"
