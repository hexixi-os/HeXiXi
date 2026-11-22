# 开发与测试指南

所有命令都在 WSL 的 Ubuntu 里执行，仓库在 `~/HeXiXi`。Windows 里用 `wsl -d Ubuntu` 进入。

## 1. 项目怎么运转

两个 Rust 程序加一张磁盘镜像，跑在 QEMU 虚拟机里：

```
QEMU（RISC-V64，-machine virt）
 ├─ OpenSBI 固件        QEMU 自带，负责启动、定时器、关机
 ├─ 内核 os.bin         Rust，S 态，加载在 0x80200000
 ├─ 磁盘 disk.img       EXT4 评测盘，virtio 块设备
 └─ 用户程序            busybox、测试程序等，U 态，由内核加载
```

启动流程：

1. 内核入口 `rust_main`（`os/src/main.rs`）读设备树、初始化内存、陷阱、时钟。
2. 探测磁盘是不是 EXT4。
3. 创建第一个进程。盘里有 `oscomp_runner` 就启动它，否则看有没有 `run_tests`（启动 `autorun`），都没有才启动 `initproc`。
4. `oscomp_runner` 在盘根目录找 `*_testcode.sh`，用盘里的 `busybox sh` 逐个执行。
5. 第一个进程以 0 退出，内核就让 QEMU 关机。

## 2. 环境

已装好，全在用户目录，不占 C 盘（WSL 的 Ubuntu 在 D 盘）。

| 工具 | 版本 | 位置 | 用途 |
|---|---|---|---|
| Rust nightly | `nightly-2022-08-05` | `~/.rustup` | 编译内核和用户程序，版本由 `rust-toolchain.toml` 锁定 |
| Rust stable | 最新 | `~/.rustup` | 编译宿主工具 `easy-fs-fuse`，安装 `cargo-binutils` |
| cargo-binutils | 0.3.3 | `~/.cargo/bin` | 提供 `rust-objcopy`（ELF 转 `os.bin`） |
| QEMU | 10.2.1（`qemu-system-riscv` 包） | 系统 | 跑内核 |
| gdb | `gdb-multiarch`，软链为 `riscv64-unknown-elf-gdb` | 系统 | 调试 |
| musl 交叉编译器 | GCC 11.2.1 | `~/toolchains/riscv64-linux-musl-cross` | 编译 busybox、lua 等测试程序 |
| cmake | 3.31.6 | `~/toolchains/cmake-*` | 构建 basic 用例 |
| 官方测试套件 | 分支 `2025_multiarch` | `~/testsuits` | 测试源码和评测脚本 |
| 构建产物 | | `~/oscomp-work/<组>/` | 各组的 `disk.img` 和日志 |

镜像源配在 `~/.cargo/config.toml`（USTC，并开启 `sparse-registry`）。
这些不用手动装：`tools/oscomp/setup.sh` 会补齐测试套件、交叉编译器和 cmake；系统包见 [BUILD.md](BUILD.md)。

## 3. 内核模块

内核在 `os/src/`，约 7700 行：

| 模块 | 做什么 |
|---|---|
| `main.rs`、`entry.asm` | 入口，启动顺序 |
| `fdt.rs` | 解析设备树，得到内存大小 |
| `mm/` | 内存：物理页分配 `frame_allocator`、内核堆 `heap_allocator`、Sv39 页表 `page_table`、进程地址空间 `memory_set`（解析 ELF、mmap、brk） |
| `trap/` | 陷阱：用户态进内核的入口（系统调用、缺页、时钟中断），`trap.S` 保存恢复寄存器 |
| `task/` | 进程和线程：`process.rs` 里有 fork/exec，`manager`/`processor` 是单核 FIFO 调度器，`signal.rs` 管信号 |
| `syscall/` | 系统调用，`mod.rs` 是分发表，按类分在 `fs.rs`、`process.rs`、`signal.rs`、`thread.rs`、`sync.rs` |
| `fs/` | `ext4.rs` 只读 EXT4；`mod.rs` 有内存 overlay（写入的文件存在内核堆里）和 `/proc`、`/dev/null` 等虚拟文件；`pipe.rs` 管道；`stdio.rs` 标准输入输出 |
| `drivers/block/` | virtio 块设备驱动 |
| `sync/` | 互斥锁、信号量、条件变量 |
| `boards/` | 板级常量 |

## 4. 日常开发

### 第一次：构建 basic 测试盘并打分

```bash
cd ~/HeXiXi
tools/oscomp/setup.sh                 # 幂等，已有的会跳过
tools/oscomp/build_runner.sh
tools/oscomp/build_busybox.sh
tools/oscomp/build_tests.sh
tools/oscomp/make_official_image.sh   # 产出 ~/oscomp-work/official/disk.img
make -C os build
tools/oscomp/run_group.sh ~/oscomp-work/official/disk.img 256M 200 ~/oscomp-work/official/basic.log
tools/oscomp/score_basic.sh ~/oscomp-work/official/basic.log
```

`run_group.sh` 参数依次是：磁盘镜像、内存、超时秒数、日志路径。它跑完会汇总组标记、是否正常关机、有没有 panic。

### 改了什么，要重做什么

| 改动 | 需要执行 |
|---|---|
| 内核（`os/`） | `make -C os build`，然后重新 `run_group.sh`，不用重新打盘 |
| `oscomp_runner.rs` | `build_runner.sh`，再重新打盘（`make_official_image.sh` 等） |
| 测试程序或脚本 | 重新构建并打盘 |

### 调试

- **看日志**：内核用 `println!` 输出到串口，最直接。
- **内核崩溃**：会打印 `[kernel] PANIC ...`、最后几个系统调用号和一段回溯。把回溯地址翻成源码行：
  ```bash
  addr2line -f -C -e os/target/riscv64gc-unknown-none-elf/release/os 0x8021ae0e
  ```
- **缺系统调用**：日志里搜 `unsupported syscall`，内核会打印调用号并返回 ENOSYS。补功能主要靠这条线索。
- **gdb**：
  ```bash
  GDB=1 tools/oscomp/run_group.sh <disk.img>     # 终端 1：QEMU 停在第一条指令等 gdb
  make -C os gdbclient                           # 终端 2：连上去
  ```
  注意 `make -C os gdbserver` 启动的 QEMU **没有挂磁盘**，只能调内核启动阶段；要带盘调试用上面的 `GDB=1`。

### 在本地镜像上跑 usertests

这个分支默认被 `oscomp_runner` 接管，`make run TEST=1` 不生效。做法见 [BUILD.md](BUILD.md)。

## 5. 测试原理

**评测只看串口输出**，不检查内核内部状态。

每组测试盘里有一个 `xxx_testcode.sh`，脚本自带标记：

```
#### OS COMP TEST GROUP START basic ####
...各测试程序的输出...
#### OS COMP TEST GROUP END basic ####
```

**basic 组怎么判分**（已验证）：每个用例打印 `========== START test_xxx ==========` 和 `END`，中间是它的输出。官方判分器 `test_runner.py` 把日志切成一段段，交给对应的 `*_test.py`，里面是若干条断言（例如第一行必须恰好是 `  I am test_echo.`）。**总分是通过的断言数**，所以 96/102 表示 102 条断言过了 96 条。`score_basic.sh` 就是调用它。

**其他组各有各的格式**：

| 组 | 看什么 |
|---|---|
| lua | 每个脚本打印 `testcase lua xxx success` |
| libc-test、ltp | 每个用例的 Pass / TPASS 数 |
| lmbench、unixbench、iozone | 有没有输出数字结果 |
| iperf、netperf | 网络测试是否成功（目前全失败，没有网卡） |

**注意**：「跑到 END 标记」只说明脚本走完了，不等于得分。每组的真实得分要按上表的格式去数。目前只有 basic 有可靠的打分工具。

## 6. 给新的测试组做盘

套路都一样，参考 `tools/oscomp/build_lua.sh`：

1. 用 musl 交叉编译器（`env.sh` 里的 `$CC`，必须 `-static -no-pie`）编出测试程序。
2. 建一个 rootfs 目录，放入：`$SUITE/scripts/<组>/` 下的脚本、测试程序、busybox、`oscomp_runner`、`tmp/`、`var/tmp/`。盘是**平铺**布局，都放根目录。
3. 调用 `mkimage <rootfs> <输出.img> <大小MB> <卷标>` 生成 EXT4 镜像（不需要 root）。
4. 用 `run_group.sh` 启动。lmbench 要 1G 内存，其余 256M 够用。

## 7. 踩过的坑

| 现象 | 原因和处理 |
|---|---|
| 官方测试套件 clone 到的目录不对 | 必须是 `2025_multiarch` 分支；`main`、`final-2025`、`final-2026` 是别的赛题。GitHub 上这个分支常报 `invalid index-pack output`，用 gitee 镜像 |
| `cargo` 报 `requires -Z sparse-registry` | 在 `~/.cargo/config.toml` 加 `[unstable] sparse-registry = true` |
| `make env` 里装 `cargo-binutils` 失败 | 旧 nightly 编不动它，改用 `cargo +stable install cargo-binutils --version 0.3.3 --locked` |
| 下载工具链中途断开 | 代理会截断大文件，脚本里用 `curl -C -` 断点续传 |
| 系统没有 cmake，又不想 sudo | 用 Kitware 预编译包解压即用，`setup.sh` 已处理 |
| exec 时内核 panic：`range end index ... out of range` | EXT4 稀疏文件的"洞"被当成文件结尾，已在 `os/src/fs/ext4.rs` 修复 |
| 本地 `make run` 直接退出，没有 shell | `oscomp_runner` 先于 `initproc` 被选中，见 [BUILD.md](BUILD.md) |
| 不要在 `/mnt/d/...` 下编译 | 跨文件系统慢好几倍，仓库放在 WSL 自己的 `~/HeXiXi` |
