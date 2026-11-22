# 构建与运行

这份讲环境安装、编译内核、在本地镜像上跑 usertests。
评测测试盘的构建、运行和判分见 [开发与测试指南](开发与测试指南.md)。

## 支持环境

- Ubuntu 22.04 及以上，原生或 WSL2（实测 Ubuntu 26.04）。
- 不支持原生 Windows，请用 WSL2。macOS 需要自己装 `riscv64-elf-gcc` 和 QEMU，没验证过。
- 仓库请放在 WSL 自己的文件系统里（如 `~/HeXiXi`），不要放 `/mnt/d/...`，跨文件系统编译慢很多。

## 一次性安装

```bash
sudo apt update
sudo apt install -y build-essential curl git make tmux \
  qemu-system-riscv gcc-riscv64-linux-gnu gcc-riscv64-unknown-elf \
  gdb-multiarch binutils-riscv64-unknown-elf
sudo ln -sf /usr/bin/gdb-multiarch /usr/local/bin/riscv64-unknown-elf-gdb   # Makefile 里调用的是这个名字

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

- 较旧的 Ubuntu（如 24.04）里 QEMU 的包名是 `qemu-system-misc`。
- `rust-toolchain.toml` 会自动拉取 `nightly-2022-08-05` 和所需组件，不用手动装。

### cargo 镜像源（国内网络必需）

`~/.cargo/config.toml`：

```toml
[source.crates-io]
replace-with = 'ustc-sparse'

[source.ustc-sparse]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"

[net]
git-fetch-with-cli = true

[unstable]
sparse-registry = true      # 锁定的 cargo 1.64 需要显式开启
```

### cargo-binutils

`make env` 会用旧 nightly 去装它，会失败。先用 stable 装好：

```bash
cargo +stable install cargo-binutils --version 0.3.3 --locked
```

### GitHub 不通时

```bash
git config --global url."https://gitee.com/".insteadOf "https://github.com/"
```

`os/Cargo.toml` 和 `user/Cargo.toml` 里的依赖已经固定为 Gitee 地址，这条只影响其他间接依赖。

## 编译

```bash
make -C os build        # 编译内核 + 用户程序 + 打包 easy-fs 镜像
make -C os clean
```

| 产物 | 路径 |
|---|---|
| 内核镜像 | `os/target/riscv64gc-unknown-none-elf/release/os.bin` |
| easy-fs 镜像 | `user/target/riscv64gc-unknown-none-elf/release/fs.img` |

评测用的 EXT4 测试盘不在这里，由 `tools/oscomp/` 的脚本生成。

## 在本地镜像上跑 usertests

**这个分支上 `make run`、`make run TEST=1` 都不会按预期工作。**
内核启动第一个进程时，盘里只要有 `oscomp_runner` 就先启动它（见 `os/src/task/mod.rs` 的 `INITPROC`），
而 `user/src/bin/` 下的程序会全部打进 `fs.img`，所以它总在。它找不到评测脚本就直接退出，内核随即关机，看起来就是「没有任何输出」。

想在本地镜像上跑 usertests，临时把它挪走：

```bash
cd ~/HeXiXi
mv user/src/bin/oscomp_runner.rs /tmp/
make -C os build TEST=1          # TEST=1 把 usertests 复制成 initproc
cd os
qemu-system-riscv64 -machine virt -m 256M -nographic -bios default \
  -kernel target/riscv64gc-unknown-none-elf/release/os.bin \
  -drive file=../user/target/riscv64gc-unknown-none-elf/release/fs.img,if=none,format=raw,id=x0 \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
```

退出 QEMU：先按 `Ctrl+A`，松开后按 `X`。

跑完务必还原，否则评测盘会缺 runner：

```bash
cd ~/HeXiXi
mv /tmp/oscomp_runner.rs user/src/bin/
rm -f user/target/riscv64gc-unknown-none-elf/release/initproc*
touch user/src/bin/initproc.rs
make -C os build
```

已实测：usertests 能启动并跑一大批用例，但跑到 `race_adder_arg` 时内核会 panic，详见 [测试状态](测试状态.md)。
不带 `TEST=1` 时 `initproc` 会启动 `user_shell`，这条路径 Martyr 没有实测。

## 常见问题

| 现象 | 原因 | 处理 |
|---|---|---|
| `Updating crates.io index` 长时间无响应 | 走的是 git 索引 | 配 USTC sparse 源（见上） |
| `usage of sparse registries requires -Z sparse-registry` | cargo 1.64 没开开关 | `[unstable] sparse-registry = true` |
| `cannot be built because it requires rustc 1.xx or newer` | 依赖选了新版 | `Cargo.lock` 已锁定；若被删，`cargo update -p regex --precise 1.7.3` |
| `this version of Cargo is older than the 2024 edition` | 旧 nightly 编不动新 crate | 这个 crate 改用 stable 安装（见 cargo-binutils） |
| `make run` 直接退出，没有 shell | `oscomp_runner` 先被启动 | 见上一节 |
| 启动后串口完全没输出 | 仓库自带的 `bootloader/rustsbi-qemu.bin` 损坏 | 已改用 `-bios default` + `-kernel`，不要换回去 |
| 输入命令时首字符丢失 | 管道一次性灌入时的串口时序问题 | 在终端里手敲 |

## 版本锁定

仓库提交了三个 `Cargo.lock`（`os/`、`user/`、`easy-fs-fuse/`），是刻意的：
工具链固定在 2022 年的 nightly，而 crates.io 上的依赖在持续升级，不锁版本随时会因 rustc 版本过低而编不过。
**改了依赖，请把 lockfile 一起提交。**

## 关于根目录的 Dockerfile

它是 `main` 分支遗留的旧环境（QEMU 7.0、stable 工具链），和这个分支要求的 `nightly-2022-08-05` 对不上，当前不使用。
