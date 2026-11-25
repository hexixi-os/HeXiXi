# 构建与运行

支持 Ubuntu 22.04+（原生或 WSL2，不支持原生 Windows；WSL 请将仓库放在 Linux 文件系统内如 `~/HeXiXi`，避免 `/mnt/d/...`）。

## 1. 环境安装

### 实测工具链版本

| 组件 | 版本 | 说明 |
|---|---|---|
| Rust（RISC-V64 内核与用户态） | `nightly-2022-08-05` | 由 `rust-toolchain.toml` 自动锁定 |
| Rust（LoongArch64） | `1.98.1` | 目标 `loongarch64-unknown-none-softfloat` |
| Rust（宿主工具） | `stable` | 构建 `easy-fs-fuse` 与 `cargo-binutils 0.3.3` |
| QEMU | 8.2.2 / 10.2.1 | 使用自带 OpenSBI（`-bios default`） |
| 交叉编译器 / 工具 | `gcc-riscv64-linux-gnu`、`musl.cc` GCC 11.2.1、CMake 3.31.6 | 本地模拟盘用 `tools/oscomp/setup.sh` 自动准备 |

### 系统包与 Rust

```bash
sudo apt update
sudo apt install -y build-essential curl git make tmux \
  qemu-system-riscv gcc-riscv64-linux-gnu gcc-riscv64-unknown-elf \
  gdb-multiarch binutils-riscv64-unknown-elf
sudo ln -sf /usr/bin/gdb-multiarch /usr/local/bin/riscv64-unknown-elf-gdb

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
cargo +stable install cargo-binutils --version 0.3.3 --locked
```

> 较旧的 Ubuntu（如 24.04）中 QEMU 包名为 `qemu-system-misc`。

### 国内 Cargo 镜像源（可选）

`~/.cargo/config.toml`（锁定的 Cargo 1.64 需显式开启 `sparse-registry`）：

```toml
[source.crates-io]
replace-with = 'ustc-sparse'

[source.ustc-sparse]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"

[net]
git-fetch-with-cli = true

[unstable]
sparse-registry = true
```

仓库提交了 `os/`、`user/`、`easy-fs-fuse/` 三个 `Cargo.lock` 以锁定旧版 nightly 兼容的依赖，修改依赖时请同步更新 lockfile。

## 2. 构建内核

### RISC-V64（单核 / 双核）

```bash
make all                         # 评测内核 kernel-rv（单核，内嵌 oscomp_runner）
make all SMP=1                   # 双核评测内核（依赖 OpenSBI HSM/RFENCE）
make -C os build                 # 开发构建：os.bin + easy-fs 镜像 fs.img
make -C os clean
```

可选构建参数：
- `PAGE_CACHE_POLICY=lru|adaptive|none`（默认 `lru`）
- `ELF_STARTUP_STATS=1`（开启 ELF/fork 启动统计）
- `KERNEL_OUT=<path>`（指定输出路径）

### LoongArch64（单核）

```bash
rustup toolchain install 1.98.1 --profile minimal
rustup target add --toolchain 1.98.1 loongarch64-unknown-none-softfloat
make kernel-la                   # 产出 ELF 格式 kernel-la
```

## 3. 运行 easy-fs `usertests`

本分支在 `fs.img` 包含 `oscomp_runner` 时会优先启动 runner，导致 `make -C os run TEST=1` 直接退出。可用独立 easy-fs 镜像将 `usertests` 作为 `initproc` 启动：

```bash
work=$(mktemp -d)
mkdir -p "$work/names" "$work/root"
(cd user && cargo build --release --bins)
user_bins=user/target/riscv64gc-unknown-none-elf/release
for app in $(sed -n 's/^    ("\([a-z_0-9]*\)\\0".*/\1/p' user/src/bin/usertests.rs | sort -u); do
    touch "$work/names/$app"
    cp "$user_bins/$app" "$work/root/$app"
done
touch "$work/names/initproc"
cp "$user_bins/usertests" "$work/root/initproc"
(cd easy-fs-fuse && cargo +stable run --release -- -s "$work/names/" -t "$work/root/")
make all KERNEL_OUT="$work/kernel-rv"
qemu-system-riscv64 -machine virt -m 256M -nographic -bios default \
  -kernel "$work/kernel-rv" -snapshot -no-reboot \
  -drive "file=$work/root/fs.img,if=none,format=raw,id=x0" \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
```

## 4. 常见问题

| 现象 | 原因与处理 |
|---|---|
| `Updating crates.io index` 卡住 / 报 `-Z sparse-registry` | 按上文配置 USTC sparse 源并加上 `[unstable] sparse-registry = true` |
| `make env` 安装 `cargo-binutils` 失败 | 先用 `cargo +stable install cargo-binutils --version 0.3.3 --locked` |
| 依赖提示需要新版 rustc | 使用已提交的 `Cargo.lock`；若重建，需 `cargo update -p regex --precise 1.7.3` |
| `make -C os run` 直接退出 | `fs.img` 含 `oscomp_runner` 被优先启动，见第 3 节 |
