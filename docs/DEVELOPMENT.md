# 开发与测试指南

## 1. 启动流程与内核模块

QEMU 加载 OpenSBI（RISC-V64）或直接加载 ELF（LoongArch64）后进入 `rust_main`（`os/src/main.rs`）：

1. 解析 DTB（RISC-V64）或 `fw_cfg`/内存布局（LoongArch64），初始化内核堆与物理页帧分配器。
2. 初始化陷阱、时钟与（启用 `SMP=1` 时的）从核启动。
3. 探测根块设备：EXT4 官方盘启动内嵌 `oscomp_runner`，平铺盘加载根目录 runner，easy-fs 盘按 `run_tests` 或 `initproc` 启动。
4. 初始化运行时目录、网络栈并进入调度；首个进程以 0 退出后执行平台关机。

### `os/src/` 目录一览

| 模块 | 职责 |
|---|---|
| `arch/` | RISC-V64（启动、SBI、SMP、上下文/陷阱汇编）与 LoongArch64 架构抽象 |
| `boards/` | QEMU `virt` 板级常量与退出句柄（`qemu.rs`、`loongarch_virt.rs`） |
| `fdt.rs` | 设备树解析（RAM 上界、启动参数 `oscomp.group=` / `oscomp.legacy=`、SMP hart） |
| `mm/` | 物理帧分配、内核堆、页表、VMA、按需 ELF 装载、CoW 与安全用户态拷贝 |
| `trap/` | 用户态陷阱分发（系统调用、缺页、时钟中断）与 `TrapContext` |
| `task/` | 进程/线程控制块、fork/exec、调度器、信号状态与跨核 TLB 失效 |
| `syscall/` | Linux 系统调用实现（`fs`、`process`、`signal`、`thread`、`futex`、`sync`、`net`、`epoll`） |
| `fs/` | VFS 挂载表、只读 EXT4、内存 Overlay 与 `tmpfs`、EasyFS 持久化后端（`persistent/`）、页缓存、符号链接、管道、设备与 `/proc` |
| `net/` | smoltcp IPv4 TCP/UDP 协议栈、回环与 socket 状态机 |
| `drivers/` | VirtIO MMIO/PCI 块设备（含第二块数据盘）与网卡驱动 |
| `sync/` | 全局内核票号锁（SMP）、`UPSafeCell`、互斥锁、信号量、条件变量 |

### 运行环境要点

- **挂载与文件系统**：支持 `ext4`、`tmpfs`、`proc`、`devtmpfs`、`easyfs` 挂载与 `mount`/`umount2`（含 `MNT_DETACH`、`EBUSY`、`EXDEV`、`statfs`/`fstatfs`）。EXT4 启动自动挂载 `/dev/shm`、`/tmp`、`/var/tmp`（保留已有非空内容）并补齐 `/lib`↔`/lib64` 兼容链接。
- **设备与 `/proc`**：`/dev/{null,zero,random,urandom,tty,console}`；`/proc/self` 与 `/proc/<pid>` 提供 `cwd`、`exe`、`cmdline`、`stat`、`status`，及 `meminfo`、`cpuinfo`、`mounts`、`page_cache`。
- **同步与栈**：`MAP_SHARED` 匿名/文件映射上的跨进程共享 `futex` 与 `clear_child_tid` 按物理页帧唤醒；双架构任务内核栈统一 16 KiB（含保护页）。

## 2. 调试

- **崩溃定位**：panic 会打印最后 8 条系统调用号与调用栈地址，用 `addr2line` 还原行号：
  ```bash
  addr2line -f -C -e os/target/riscv64gc-unknown-none-elf/release/os <地址>
  ```
- **缺失系统调用**：串口搜索 `unsupported syscall`（每轮最多打印 40 条，返回 `ENOSYS`）。
- **GDB**：带磁盘调试通过 `run_group.sh` 启动：
  ```bash
  GDB=1 tools/oscomp/run_group.sh <disk.img>    # 终端 1
  make -C os gdbclient                          # 终端 2
  ```

## 3. 专项功能回归

所有脚本在仓库根目录执行，通过独立测试盘自动校验：

| 回归项 | 命令 |
|---|---|
| EasyFS 宿主单元测试 | `cargo +stable test --manifest-path easy-fs/Cargo.toml` |
| EXT4 区间读取与页缓存（宿主） | `bash tools/oscomp/test_ext4.sh [sdcard-rv.img]` |
| ELF 入口契约与组选择（双架构） | `bash tools/oscomp/test_exec_entry.sh <riscv64\|loongarch64> /tmp/hexixi-exec.log` |
| VFS、挂载、设备与共享 `futex`（双架构） | `bash tools/oscomp/test_vfs_abi.sh <riscv64\|loongarch64> /tmp/hexixi-vfs.log` |
| IPv4 TCP/UDP、epoll 与 VirtIO 网络（双架构） | `bash tools/oscomp/test_net_abi.sh [--virtio] <riscv64\|loongarch64\|all>` |
| EasyFS 持久化数据盘（双架构） | `bash tools/oscomp/test_persistent_fs.sh <riscv64\|loongarch64\|all> /tmp/hexixi-persist` |
| basic ABI / `mmap` / 按需 ELF / CoW / Overlay | `bash tools/oscomp/test_{basic_abi,mmap_vma,elf_demand,fork_cow,overlay_fs}.sh` |
| 页缓存策略对照 | `bash tools/oscomp/test_page_cache_policy.sh <scenario> <log>` |
| RISC-V64 双核 SMP | `make all SMP=1 && KERNEL="$PWD/kernel-rv" bash tools/oscomp/test_smp.sh /tmp/hexixi-smp.log` |
| LoongArch64 启动与 `clone` | `make kernel-la && bash tools/oscomp/test_loongarch_boot.sh /tmp/hexixi-la.log` |
| 动态 ELF / pthread（需先准备 fixture） | `bash tools/oscomp/test_{elf_dynamic,pthread_abi}.sh <fixture-root> <log>` |

### 构建动态 ELF / pthread 测试 fixture

```bash
fixture=$(mktemp -d)
mkdir -p "$fixture/lib"
musl_cc=/path/to/riscv64-buildroot-linux-musl-gcc
glibc_cc=riscv64-linux-gnu-gcc

"$musl_cc" -fPIE -pie -O2 tools/oscomp/tests/elf_hello.c -o "$fixture/hello-musl"
"$glibc_cc" -fPIE -pie -O2 tools/oscomp/tests/elf_hello.c -o "$fixture/hello-glibc"
"$glibc_cc" -fno-pie -no-pie -Wl,-Ttext-segment=0x50000000 -O2 tools/oscomp/tests/elf_hello.c -o "$fixture/hello-glibc-exec"
"$musl_cc" -fPIE -pie -pthread -O2 tools/oscomp/tests/pthread_abi.c -o "$fixture/pthread-musl"
"$glibc_cc" -fPIE -pie -pthread -O2 tools/oscomp/tests/pthread_abi.c -o "$fixture/pthread-glibc"

install -m755 "$("$musl_cc" -print-file-name=libc.so)" "$fixture/lib/ld-musl-riscv64-sf.so.1"
for lib in ld-linux-riscv64-lp64d.so.1 libc.so.6 libpthread.so.0 libm.so.6 libresolv.so.2; do
    install -m755 "$("$glibc_cc" -print-file-name="$lib")" "$fixture/lib/$lib"
done
debugfs -R "dump /musl/busybox $fixture/busybox" /path/to/sdcard-rv.img && chmod +x "$fixture/busybox"
```

## 4. 本地模拟盘（basic 快速离线调试）

```bash
bash tools/oscomp/setup.sh
bash tools/oscomp/build_runner.sh
bash tools/oscomp/build_busybox.sh
bash tools/oscomp/build_tests.sh
bash tools/oscomp/make_local_image.sh
make -C os build
bash tools/oscomp/run_group.sh ~/oscomp-work/local/disk.img 256M 200 ~/oscomp-work/local/basic.log
bash tools/oscomp/score_basic.sh ~/oscomp-work/local/basic.log
```

> 本地模拟盘为平铺布局，仅用于离线调试，最终成绩以官方原盘为准。
