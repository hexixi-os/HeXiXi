# 开发与测试指南

所有命令都在 WSL 的 Ubuntu 里执行，仓库在 `~/HeXiXi`。Windows 里用 `wsl -d Ubuntu` 进入。
各工具和测试套件的官方地址见 [参考链接](REFERENCES.md)。

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
3. 创建第一个进程。官方布局的 EXT4 盘使用内嵌的 `oscomp_runner`。平铺盘可提供自己的 runner，教学盘由 `run_tests` 或默认 `initproc` 选择入口。
4. 官方盘的 runner 切到 `/musl`，用该目录下的 `busybox sh` 执行 `basic_testcode.sh`。平铺盘由原 runner 在根目录查找脚本。
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

### 第一次：构建 basic 本地模拟盘并打分

```bash
cd ~/HeXiXi
tools/oscomp/setup.sh                 # 幂等，已有的会跳过
tools/oscomp/build_runner.sh
tools/oscomp/build_busybox.sh
tools/oscomp/build_tests.sh
tools/oscomp/make_local_image.sh   # 产出 ~/oscomp-work/local/disk.img
make -C os build
tools/oscomp/run_group.sh ~/oscomp-work/local/disk.img 256M 200 ~/oscomp-work/local/basic.log
tools/oscomp/score_basic.sh ~/oscomp-work/local/basic.log
```

`run_group.sh` 参数依次是：磁盘镜像、内存、超时秒数、日志路径。它跑完会汇总组标记、是否正常关机、有没有 panic。

### 改了什么，要重做什么

| 改动 | 需要执行 |
|---|---|
| 内核（`os/`） | `make -C os build`，然后重新 `run_group.sh`，不用重新打盘 |
| `oscomp_runner.rs` | 官方盘走 `make all`，更新内嵌 runner。平铺盘走 `build_runner.sh`，再重新打盘 |
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

## 6. 官方评测盘与本地模拟盘

**我们日常测的是本地模拟盘，不是官方评测盘，两者有差异，模拟盘上的分数不等于评测机分数。**

| 方面 | 官方评测盘 | 本地模拟盘 |
|---|---|---|
| 来源 | 2025 初赛发布的 `sdcard-rv.img.xz` 解压使用 | `mkfs.ext4 -d` 从目录生成，64 MB |
| 根目录 | 测试文件在 `musl/` 和 `glibc/` 子目录 | 平铺：脚本、程序、busybox 都在根目录 |
| 脚本里的标记 | `#### OS COMP TEST GROUP START basic-musl ####`（带 `-musl`/`-glibc` 后缀） | 无后缀 |
| `oscomp_runner` | `make all` 内嵌到评测内核 | 放在测试盘根目录 |
| 编译器 | 容器里的 musl GCC 12.2 / glibc 交叉编译器 | musl.cc 的 GCC 11.2.1，只有 musl 一套 |
| QEMU | 9.2.1，带网卡和第二块盘 `disk.img` | 10.2.1，只有一块盘 |

官方的运行命令见 [oscomp-evaluation.md](oscomp-evaluation.md)。

原盘 basic-musl 回归使用 `tools/oscomp/run_official_basic.sh`。全部 24 项由 `tools/oscomp/run_official_matrix.py` 独立启动并记录串口、判分和运行元数据。下载与复跑步骤见 [官方盘回归](oscomp-evaluation.md#官方盘回归)和[完整评测矩阵](oscomp-evaluation.md#完整评测矩阵)，实测结果见 [测试状态](TEST_STATUS.md)。

### 从旧版 Docker 环境生成评测布局（历史记录）

需要 Docker Desktop（在 Windows 里运行，WSL 里没有装 docker）。构建产物约 1 GB 压缩、4 GB 解压，耗时十几分钟，注意 D 盘空间。官方套件 `~/testsuits` 要先用 `setup.sh` 准备好。

```powershell
docker pull alicesama/os-contest-image:v0.2
docker volume create oscomp-code
wsl -d Ubuntu -e bash -c "tar -C ~/testsuits --exclude=.git -cf - ." | docker run --rm -i -v oscomp-code:/code --entrypoint bash alicesama/os-contest-image:v0.2 -c "tar -C /code -xf -"
```

官方套件的 `iperf/` 目录没有 `Makefile`，官方 `clean` 会失败，所以先放一个只含空 `clean` 的占位文件，再跑官方 `make`（要 `--privileged`，因为要 `mount`）：

```powershell
docker run -d --name oscomp-build --privileged -v oscomp-code:/code -w /code --entrypoint bash alicesama/os-contest-image:v0.2 -c "printf 'clean:\n\t@true\n' > iperf/Makefile; make 2>&1 | tee /code/official-build.log; echo BUILD_EXIT=`${PIPESTATUS[0]} >> /code/official-build.log"
```

构建完成后日志末尾是 `BUILD_EXIT=0`，产物是卷里的 `/code/sdcard-riscv64.img.gz`。取回 WSL：

```powershell
docker run --rm -v oscomp-code:/code --entrypoint cat alicesama/os-contest-image:v0.2 /code/sdcard-riscv64.img.gz | wsl -d Ubuntu -e bash -c "mkdir -p ~/oscomp-work/official-real && cat > ~/oscomp-work/official-real/sdcard-riscv64.img.gz"
```

```bash
cd ~/oscomp-work/official-real && gunzip -k sdcard-riscv64.img.gz
truncate -s 64M disk.img       # 官方命令里的第二块盘，随便给个空文件
qemu-system-riscv64 -machine virt -kernel ~/HeXiXi/os/target/riscv64gc-unknown-none-elf/release/os.bin   -m 1G -nographic -smp 1 -bios default   -drive file=sdcard-riscv64.img,if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0   -no-reboot -device virtio-net-device,netdev=net -netdev user,id=net -rtc base=utc   -drive file=disk.img,if=none,format=raw,id=x1 -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
```

Docker 环境和 WSL 环境是两套互相独立的环境，只通过文件传递产物：

| | Docker 容器 | WSL Ubuntu |
|---|---|---|
| 作用 | 只负责生成官方测试盘 | 编译内核、跑 QEMU、日常开发 |
| 系统 | Debian 12（官方镜像） | Ubuntu 26.04 |
| 工具 | musl/glibc 交叉编译器、QEMU 9.2.1，没有 Rust | Rust、QEMU 10.2.1、`~/HeXiXi` |
| 数据 | Docker 卷 `oscomp-code` | `~/oscomp-work/`、`~/testsuits` |

官方盘只需生成一次；改内核不用再碰 Docker。清理空间：`docker volume rm oscomp-code`（官方盘已另存在 WSL 时才可删）。

## 7. 给新的测试组做盘

套路都一样，参考 `tools/oscomp/build_lua.sh`：

1. 用 musl 交叉编译器（`env.sh` 里的 `$CC`，必须 `-static -no-pie`）编出测试程序。
2. 建一个 rootfs 目录，放入：`$SUITE/scripts/<组>/` 下的脚本、测试程序、busybox、`oscomp_runner`、`tmp/`、`var/tmp/`。本地模拟盘是**平铺**布局，都放根目录（真实官方盘不是，见第 6 节）。
3. 调用 `mkimage <rootfs> <输出.img> <大小MB> <卷标>` 生成 EXT4 镜像（不需要 root）。
4. 用 `run_group.sh` 启动。lmbench 要 1G 内存，其余 256M 够用。

## 8. 踩过的坑

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
