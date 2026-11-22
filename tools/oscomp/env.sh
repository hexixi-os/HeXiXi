# env.sh — tools/oscomp 下所有脚本共用的环境（被 source，不要直接执行）
#
# 目录约定（均可用同名环境变量覆盖）：
#   SUITE      官方测试套件源码        $HOME/testsuits        （setup.sh 会 clone）
#   TOOLCHAIN  riscv64 musl 交叉编译器  $HOME/toolchains/riscv64-linux-musl-cross/bin
#   CMAKE_BIN  cmake（basic 组需要）    $HOME/toolchains/cmake-3.31.6-linux-x86_64/bin
#   WORK       所有构建产物与测试盘      $HOME/oscomp-work
#
# 每个测试组一张盘：$WORK/<组>/disk.img；oscomp_runner 放在 $WORK/fsroot/target/。
# 盘的根目录是平铺布局：busybox、*_testcode.sh、各测试二进制、oscomp_runner 都在 /。

_ENV_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$_ENV_DIR/../.." && pwd)"

SUITE=${SUITE:-$HOME/testsuits}
TOOLCHAIN=${TOOLCHAIN:-$HOME/toolchains/riscv64-linux-musl-cross/bin}
CMAKE_BIN=${CMAKE_BIN:-$HOME/toolchains/cmake-3.31.6-linux-x86_64/bin}
WORK=${WORK:-$HOME/oscomp-work}
mkdir -p "$WORK"
RUNNER=${RUNNER:-$WORK/fsroot/target/oscomp_runner}
PREFIX="$TOOLCHAIN/riscv64-linux-musl-"
CC="${PREFIX}gcc"
STRIP="${PREFIX}strip"

# rustup / cargo（内核与 runner 的构建需要）
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export PATH="$TOOLCHAIN:$CMAKE_BIN:$PATH"
export LC_ALL=C

die() { echo "ERROR: $*" >&2; exit 1; }

need_suite() {
    [ -d "$SUITE/scripts" ] || die "找不到测试套件 $SUITE，请先运行 tools/oscomp/setup.sh"
}
need_cc() {
    [ -x "$CC" ] || die "找不到交叉编译器 $CC，请先运行 tools/oscomp/setup.sh"
}
need_runner() {
    [ -f "$RUNNER" ] || die "找不到 $RUNNER，请先运行 tools/oscomp/build_runner.sh"
}
need_busybox() {
    [ -f "$SUITE/busybox/busybox" ] || die "找不到 busybox，请先运行 tools/oscomp/build_busybox.sh"
}

# mkimage <rootfs 目录> <输出 img> <大小 MB> <卷标>
# 用 mkfs.ext4 -d 直接从目录生成镜像：不需要 root，权限位取自 rootfs 里的文件。
mkimage() {
    local root=$1 img=$2 mb=$3 label=$4
    mkdir -p "$(dirname "$img")"
    rm -f "$img"
    truncate -s "${mb}M" "$img"
    mkfs.ext4 -q -F -L "$label" -d "$root" "$img"
    e2fsck -fn "$img" >/dev/null 2>&1 || echo "WARN: e2fsck 对 $img 报告了问题"
    echo "image: $img ($(du -h --apparent-size "$img" | cut -f1), $(find "$root" -type f | wc -l) files)"
}

# stage_common <rootfs 目录>：每个组盘都需要的公共文件
stage_common() {
    local root=$1
    need_runner; need_busybox
    mkdir -p "$root/tmp" "$root/var/tmp"
    install -m755 "$SUITE/busybox/busybox" "$root/busybox"
    install -m755 "$RUNNER" "$root/oscomp_runner"
}
