# 开发与测试指南

## 1. 启动流程与内核模块

QEMU 加载 OpenSBI（RISC-V64）或直接加载 ELF（LoongArch64）后进入 `rust_main`（`os/src/main.rs`）：

1. 解析 DTB（RISC-V64）或 `fw_cfg`/内存布局（LoongArch64），初始化动态内核堆与物理页帧分配器。
2. 初始化陷阱、时钟与（启用 `SMP=1` 时的）从核启动。
3. 探测根块设备是否为 EXT4：官方布局盘启动内嵌 `oscomp_runner`，平铺盘加载根目录 runner，easy-fs 盘按 `run_tests` 或 `initproc` 启动。
4. 首个进程以 0 退出后，内核执行关机。

### `os/src/` 目录一览

| 模块 | 职责 |
|---|---|
| `arch/` | RISC-V64（启动、SBI、SMP、上下文/陷阱汇编）与 LoongArch64 架构抽象层 |
| `boards/` | QEMU `virt` 板级常量与退出句柄（`qemu.rs`、`loongarch_virt.rs`） |
| `fdt.rs` | 设备树解析（RAM 上界、启动参数 `oscomp.group=`、SMP hart 列表） |
| `mm/` | 物理帧分配、动态内核堆、页表、VMA、按需 ELF 装载、CoW 与安全用户态拷贝 |
| `trap/` | 用户态陷阱分发（系统调用、缺页、时钟中断）与 `TrapContext` |
| `task/` | 进程/线程控制块、fork/exec、调度器、信号状态与跨核页表失效 |
| `syscall/` | Linux 系统调用分发与实现（`fs`、`process`、`signal`、`thread`、`futex`、`sync`） |
| `fs/` | 只读 EXT4、内存可写 Overlay、统一文件页缓存、管道、标准 I/O 与 `/proc` 虚拟文件 |
| `drivers/block/` | VirtIO MMIO（RISC-V64）与 VirtIO PCI（LoongArch64）块设备驱动 |
| `sync/` | 全局内核票号锁（SMP）、`UPSafeCell`、互斥锁、信号量、条件变量 |

## 2. 调试

- **日志**：内核直接使用 `println!` 输出至串口。
- **崩溃定位**：panic 会打印最后 8 条系统调用号与调用栈地址，用 `addr2line` 还原行号：
  ```bash
  addr2line -f -C -e os/target/riscv64gc-unknown-none-elf/release/os <地址>
  ```
- **缺失系统调用**：串口搜索 `unsupported syscall`（每轮最多打印前 40 条，返回 `ENOSYS`）。
- **GDB**：带磁盘调试需通过 `run_group.sh` 启动：
  ```bash
  GDB=1 tools/oscomp/run_group.sh <disk.img>    # 终端 1
  make -C os gdbclient                          # 终端 2
  ```

## 3. 专项功能回归

所有脚本均在仓库根目录执行，通过独立 EXT4 测试盘自动完成校验：

| 回归项 | 命令 |
|---|---|
| EXT4 区间读取与页缓存（宿主） | `bash tools/oscomp/test_ext4.sh [sdcard-rv.img]` |
| basic ABI（大偏移写、目录/unlink、clone） | `bash tools/oscomp/test_basic_abi.sh` |
| 文件 `mmap` 与 VMA 拆分 | `bash tools/oscomp/test_mmap_vma.sh` |
| ELF 按需装载 | `bash tools/oscomp/test_elf_demand.sh` |
| `fork` 写时复制（CoW） | `bash tools/oscomp/test_fork_cow.sh` |
| 页缓存策略对照 | `bash tools/oscomp/test_page_cache_policy.sh <scenario> <log>` |
| 可写内存 Overlay | `bash tools/oscomp/test_overlay_fs.sh /tmp/hexixi-overlay.log` |
| RISC-V64 双核 SMP | `make all SMP=1 && KERNEL="$PWD/kernel-rv" bash tools/oscomp/test_smp.sh /tmp/hexixi-smp.log` |
| LoongArch64 启动与 `clone` | `make kernel-la && bash tools/oscomp/test_loongarch_boot.sh /tmp/hexixi-la.log` |
| 动态 ELF（musl / glibc） | `bash tools/oscomp/test_elf_dynamic.sh <fixture-root> <log>` |
| pthread 与动态 BusyBox | `bash tools/oscomp/test_pthread_abi.sh <fixture-root> <log>` |

### 构建动态 ELF / pthread 测试 fixture

使用 RISC-V64 musl 与 glibc 交叉编译器及官方盘静态 `busybox` 准备目录后，即可运行 `test_elf_dynamic.sh` 和 `test_pthread_abi.sh`：

```bash
fixture=$(mktemp -d)
mkdir -p "$fixture/lib"
musl_cc=/path/to/riscv64-buildroot-linux-musl-gcc
glibc_cc=riscv64-linux-gnu-gcc

# 动态 ELF 用例
"$musl_cc" -fPIE -pie -O2 tools/oscomp/tests/elf_hello.c -o "$fixture/hello-musl"
"$glibc_cc" -fPIE -pie -O2 tools/oscomp/tests/elf_hello.c -o "$fixture/hello-glibc"
"$glibc_cc" -fno-pie -no-pie -Wl,-Ttext-segment=0x50000000 -O2 tools/oscomp/tests/elf_hello.c -o "$fixture/hello-glibc-exec"

# pthread 用例（额外安装 glibc 线程/数学库及动态 busybox）
"$musl_cc" -fPIE -pie -pthread -O2 tools/oscomp/tests/pthread_abi.c -o "$fixture/pthread-musl"
"$glibc_cc" -fPIE -pie -pthread -O2 tools/oscomp/tests/pthread_abi.c -o "$fixture/pthread-glibc"

install -m755 "$("$musl_cc" -print-file-name=libc.so)" "$fixture/lib/ld-musl-riscv64-sf.so.1"
for lib in ld-linux-riscv64-lp64d.so.1 libc.so.6 libpthread.so.0 libm.so.6 libresolv.so.2; do
    install -m755 "$("$glibc_cc" -print-file-name="$lib")" "$fixture/lib/$lib"
done
debugfs -R "dump /musl/busybox $fixture/busybox" /path/to/sdcard-rv.img && chmod +x "$fixture/busybox"
```

## 4. 本地模拟盘（平铺布局）

用于快速离线调试单个测试组（与真实官方盘存在目录结构与编译器差异，成绩以官方原盘为准）：

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

改动后只需增量重做：改 `os/` 重新 `make all` 或 `make -C os build`；改 `oscomp_runner.rs` 需重编内核（官方盘）或 `build_runner.sh` + 重打盘（模拟盘）。
