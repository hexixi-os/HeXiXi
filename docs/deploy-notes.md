# rCore（rCore-Tutorial-v3）本地部署说明

> **历史记录**：这是 wonder-world 2026-11-04 在自己机器上的部署笔记，系统、用户名、路径都和现在不同（当前环境见 [开发环境版本](../dev-env-info.md)）。
> 第四章「为了让它能编译改了什么」仍然有参考价值；其余部分仅供参考。当前的构建方法见 [BUILD.md](BUILD.md)。

> 部署方式：WSL2 + Ubuntu 24.04，发行版装在 **D 盘**。
> 部署日期：2026-11-04

## 一、装在哪

| 项目 | 位置 |
|---|---|
| WSL 发行版 | `Ubuntu-24.04`（WSL 2.7.12，内核 6.18） |
| 虚拟磁盘 | `D:\wsl\Ubuntu-24.04\ext4.vhdx`（初始 1.4 GB，动态增长） |
| 代码仓库 | WSL 内 `~/rCore-Tutorial-v3`（即 `\\wsl.localhost\Ubuntu-24.04\home\rc\rCore-Tutorial-v3`） |
| 登录用户 | `rc`（密码已从文档中移除） |
| 默认用户 | 已写入 `/etc/wsl.conf` 的 `[user] default=rc` |

进入方式（PowerShell）：

```powershell
wsl -d Ubuntu-24.04
```

## 二、怎么跑

```bash
cd ~/rCore-Tutorial-v3/os
make run          # 编译内核 + 用户程序 + 文件系统镜像，然后启动 QEMU
```

启动后会打印 OpenSBI 横幅，然后：

```
[kernel] Hello, world!
...
**************/
Rust user shell
>>
```

在 `>>` 后面可以敲 `ls`、`cat <文件>`、`usertests`、`exit` 等（ch8 的 shell 不支持 `help`）。

| 命令 | 作用 |
|---|---|
| `make build` | 只编译，不启动 |
| `make run` | 编译并启动交互 shell |
| `make run TEST=1` | 把 initproc 换成 `usertests`，开机自动跑全套测试 |
| `make clean` | 清构建产物 |
| `make debug` | 起 QEMU 等 gdb（需要 tmux + gdb） |

> **注意**：跑过 `make run TEST=1` 后，`user/target/.../initproc` 会被测试程序覆盖。想恢复默认 shell：
> ```bash
> cd ~/rCore-Tutorial-v3/user && rm -f target/riscv64gc-unknown-none-elf/release/initproc* && touch src/bin/initproc.rs
> cd ../os && make build
> ```

## 三、已装的依赖

| 组件 | 版本 |
|---|---|
| QEMU | 8.2.2（`qemu-system-misc`，含 `qemu-system-riscv64`） |
| RISC-V 交叉 gcc | `riscv64-linux-gnu-gcc` 13.3.0 / `riscv64-unknown-elf-gcc` 13.2.0 |
| gdb | `gdb-multiarch`（RISC-V 调试） |
| Rust | rustup 1.29.3；内核用仓库锁定的 `nightly-2022-08-05`，宿主工具用 stable 1.99 |
| 其它 | git / make / build-essential / pkg-config |

## 四、为了让它在 2026 年还能编译，改了这些地方

上游代码停在 2022 年，直接 `make run` 会连不上 GitHub / crates.io 且被新版本 crate 卡住。以下是实际改动，重装/换机时可照做。

### 1. 网络（crates.io 与 GitHub）

`~/.cargo/config.toml`：

```toml
[source.crates-io]
replace-with = 'ustc-sparse'

[source.ustc-sparse]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"

[net]
git-fetch-with-cli = true

[unstable]
sparse-registry = true      # 仓库锁定的 cargo 1.64 需要这个开关才认 sparse 源
```

git 全局改写（把 Cargo.toml 里的 GitHub git 依赖导到 Gitee）：

```bash
git config --global url."https://gitee.com/".insteadOf "https://github.com/"
```

### 2. Cargo.toml

- `os/`、`user/` 里的 git 依赖地址 `github.com` → `gitee.com`（`rcore-os/riscv`、`rcore-os/virtio-drivers`、`wyfcyx/k210-*` 在 Gitee 都有镜像）。
- 两个 manifest 都加了 `log = "=0.4.17"`（新版 `log` 要求 rustc ≥ 1.71，锁定的 nightly 只有 1.64）。
- `os/Cargo.toml` 删掉了 `k210-pac` / `k210-hal` / `k210-soc` 三个依赖（Gitee 上三者的版本互相对不上，qemu 也用不到）。

### 3. lockfile 降级

```bash
cd os && cargo update -p regex --precise 1.7.3
cd user && cargo update -p regex --precise 1.7.3
cd easy-fs-fuse && cargo update -p libc --precise 0.2.160
```

### 4. os/src/drivers/block/mod.rs

`sdcard` 模块依赖 k210 的那三个 crate，改成按 feature 门控：

```rust
mod virtio_blk;

#[cfg(feature = "board_k210")]
mod sdcard;
#[cfg(feature = "board_k210")]
pub use sdcard::SDCardWrapper;
pub use virtio_blk::VirtIOBlock;
```

### 5. os/Makefile

| 原 | 改为 | 原因 |
|---|---|---|
| `cargo install cargo-binutils` | `command -v rust-objcopy >/dev/null 2>&1 \|\| cargo install cargo-binutils --version 0.3.3` | 不联网重复装、且 0.4.0 要新 cargo |
| `cd ../easy-fs-fuse && cargo run --release` | `... cargo +stable run --release` | 宿主工具用 stable 编，绕开一堆老 crate 的 MSRV 地雷 |
| `-bios $(BOOTLOADER)` | `-bios default` | 仓库自带的 `bootloader/rustsbi-qemu.bin` 是坏的（启动无任何输出） |
| `-device loader,file=$(KERNEL_BIN),addr=$(KERNEL_ENTRY_PA)` | `-kernel $(KERNEL_BIN)` | `-bios default` 时 OpenSBI 的 next address 是 0，必须用 `-kernel` 才会跳转到 0x80200000 |

### 6. rust-objcopy / rust-objdump

`llvm-tools-preview` 只带 `llvm-objcopy`，没有 cargo-binutils 的 `rust-objcopy` 包装。已在 `~/.cargo/bin/` 放了两个 shim，负责丢掉 `--binary-architecture=riscv64`、`--arch-name=riscv64` 这类 GNU 风格参数后转发给 `llvm-objcopy` / `llvm-objdump`。

## 五、验证结果

- `make build` 通过，产物：`os/target/riscv64gc-unknown-none-elf/release/os.bin`（144 KB）、`user/target/riscv64gc-unknown-none-elf/release/fs.img`（16 MB）。
- `make run TEST=1` 启动后 usertests 全量跑起来，逐项输出绿色 `Test ... exited with code 0`（filetest / cat / exit / matrix / mpsc_sem / pipe_large_test 等）。
- 交互 shell 正常，`cat`、`exit` 等命令可执行。

## 六、已知小毛病（上游代码本身）

- 管道一次性灌入多行输入时，shell 会吞掉第一个字符（用终端手敲不受影响）。
- shell 执行不存在的程序（如 `help`）会打印大量乱码并触发 `Heap allocation error` panic —— 属于 ch8 用户态 shell 的健壮性问题，不影响内核功能。

## 七、清理 / 卸载

```powershell
wsl --terminate Ubuntu-24.04          # 关掉
wsl --unregister Ubuntu-24.04         # 彻底删除（会删掉 D:\wsl\Ubuntu-24.04 下的 vhdx）
```

`D:\wsl\ubuntu-22.04.tar`（48 MB）是之前遗留的旧文件，与本次部署无关，可删。

## 八、附：工作区权限修复记录

会话工作目录 `D:\workspace\core` 原先缺少当前账户的显式权限，导致所有命令无法启动。已通过 DSH 的诊断脚本补上完全控制项，备份与回滚脚本在 `D:\workspace\core-acl-recovery\`。
