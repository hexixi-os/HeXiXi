# 测试状态

## 2026-11-24 PR-12 动态 ELF

在 QEMU 8.2.2 上以 `execve` 启动 musl 1.2.0 PIE、glibc 2.31 PIE 和高地址 glibc 2.31 非 PIE。三个程序均经各自的加载器链接 libc，读取参数和环境变量，输出 Hello World 并以 0 退出。两个 glibc 程序同时验证堆能扩展和收缩。主程序和解释器的 `PT_LOAD` 复用 ELF 缺页与文件页缓存。解释器缺失、路径未终止、解释器格式错误及映射冲突均纳入 ELF 回归。

原盘 `basic-musl` 和 `basic-glibc` 均为 **102/102**，各 32 个用例全部满分。musl 正常关机，glibc BusyBox 仍在结束标记后以 139 退出。原盘 basic 的无依赖、无重定位程序在其声明的解释器缺失时沿用直启。basic ABI、静态 ELF 按需加载、fork COW、`mmap`/VMA 和 LRU `mixed32` 回归通过。

在 Linux 环境下复跑动态 ELF。下例使用 Bootlin RISC-V64 软浮点 musl 交叉编译器、Ubuntu RISC-V64 glibc 交叉编译器和从官方盘提取的静态 BusyBox。若更换工具链，用 `readelf -l` 确认各程序的解释器路径，按该路径放置普通文件。

```bash
fixture=$(mktemp -d)
mkdir -p "$fixture/lib"
musl_cc=/path/to/riscv64-buildroot-linux-musl-gcc
glibc_cc=riscv64-linux-gnu-gcc
source_file=tools/oscomp/tests/elf_hello.c
"$musl_cc" -fPIE -pie -O2 "$source_file" -o "$fixture/hello-musl"
"$glibc_cc" -fPIE -pie -O2 "$source_file" -o "$fixture/hello-glibc"
"$glibc_cc" -fno-pie -no-pie -Wl,-Ttext-segment=0x50000000 -O2 "$source_file" -o "$fixture/hello-glibc-exec"
install -m755 "$("$musl_cc" -print-file-name=libc.so)" "$fixture/lib/ld-musl-riscv64-sf.so.1"
for library in ld-linux-riscv64-lp64d.so.1 libc.so.6; do
    install -m755 "$("$glibc_cc" -print-file-name="$library")" "$fixture/lib/$library"
done
debugfs -R "dump /musl/busybox $fixture/busybox" /path/to/sdcard-rv.img
chmod +x "$fixture/busybox"
tools/oscomp/test_elf_dynamic.sh "$fixture" "$fixture/serial.log"
```

## 2026-11-24 PR-11 官方原盘评测基线

第一阶段基线 `c5903f1f` 在快进合入本地 `oscomp-adaptation` 前通过原盘 `basic-musl`，得分 102/102，32 个用例全部满分并正常关机。该基线与 PR-11 的评测内核均通过 basic ABI、ELF 按需加载、fork COW 和 `mmap`/VMA 回归。两者的默认 LRU 在 `mixed32` 页缓存回归中均读取 25,600 个扇区，热点未命中 96 次。

矩阵使用 `feat/oscomp-linux-compat` 的 `bb4bed09128b6020b6a20ea0cdfc4d1934c0dbd1` 内核，SHA-256 为 `d9a3d5d0d2df78debeab8acbd9be85c986a490c6d4fff9d195e0a72743554f79`。2025 初赛 RISC-V64 发布盘解压后的 SHA-256 为 `95973543db6b84a9a5e70f30da466ce292867aff5b689fb14c88dc9406e378b8`。使用 QEMU 8.2.2、单核、1 GiB 内存，每组独立启动并限制 120 秒。套件与公开判分器版本见[复跑命令](oscomp-evaluation.md#完整评测矩阵)。

24 项均启动了目标脚本。9 项正常退出，9 项在输出结束标记后以 139 退出，6 项超时。全部串口中未观察到内核 panic。表中的分数来自本地运行的公开判分器。其余组记录脚本自己的结果或日志，赛事平台总分待评测。

| 组 | musl 运行与结果 | glibc 运行与结果 | 串口观察 |
|---|---|---|---|
| basic | 正常，102/102 | 退出 139，102/102 | 两版各 32 个用例满分。glibc BusyBox 在结束标记后发生取指缺页 |
| busybox | 超时，10/53 | 超时，2/53 | 两版都停在 `dmesg` 后的 `du` 一带。分数来自超时前的输出 |
| lua | 正常，0/9 | 退出 139，0/9 | 原盘有 `test.sh`，其 shebang 指向盘上缺失的 `/bin/busybox` |
| libctest | 正常，220 项共 0 分 | 退出 139，220 项共 0 分 | 原盘有 `run-static.sh`、`run-dynamic.sh`，当前 BusyBox 报两个脚本无法执行 |
| libcbench | 正常，部分子项输出耗时 | 退出 139 | 两版都有用户态取指缺页 |
| lmbench | 超时，输出部分延迟数据 | 超时，输出部分延迟数据 | 两版都记录到 `Simple read`，此后没有完成整组 |
| unixbench | 正常 | 退出 139 | 多个子进程取指缺页退出 |
| iozone | 正常，20 项共 0 分 | 退出 139，20 项共 0 分 | 测试程序多次取指缺页，判分器没有匹配到有效吞吐量 |
| cyclictest | 正常，4 项标记 `success` | 退出 139，4 项标记 `fail` | musl 的 `hackbench` 崩溃，部分环境命令或接口缺失，带压结果无效 |
| ltp | 超时，109 个子进程退出记录 | 超时，109 个子进程退出记录 | 两版缺少 `basename`，用例名为空，随后都停在 `cgroup_regression_3_1.sh` |
| iperf | 正常，6 项标记 `fail` | 退出 139，6 项标记 `fail` | musl 报缺 `/dev/urandom` 和 `socket` 返回 `ENOSYS`，glibc 有取指缺页 |
| netperf | 正常，5 项标记 `fail` | 退出 139，5 项标记 `fail` | 两版都在子项中出现取指缺页 |

## 2026-11-24 PR-10 页缓存扫描识别

默认策略是 `lru`，可在构建时用 `PAGE_CACHE_POLICY=none|lru|adaptive` 切换。LRU 和自适应策略都最多保留 256 个 4 KiB 文件页，保护仍被映射引用的帧。自适应策略为每个打开文件记录连续读取的页数。缓存满时，从第 8 个连续页开始，未命中且近期历史中没有记录的页直接读请求的字节区间。近期淘汰页和被旁路后再次访问的页可以重新入缓存。历史最多记录 512 次淘汰或旁路事件。两种缓存策略都让 `mmap`、ELF 缺页和普通读取共享文件页。

`/proc/page_cache` 在每次打开时生成一份只读快照。`hits`、`misses` 以查找次数计，一个 `readv` 跨多个缓冲区时可能多次查找同一页。`evictions` 包含容量淘汰及内存压力回收。`bypasses` 计入关缓存、扫描旁路、没有可淘汰帧及分配失败。`refaults` 只统计近期确实被淘汰的页再次未命中，历史过期后再访问不会计入。`resident_pages` 和 `pinned_pages` 分别是当前驻留数和其中仍有缓存外引用的页数。`ext4_sectors_read` 统计已完成的 EXT4 数据和元数据扇区读取，每个扇区为 512 字节。自行比较吞吐时应固定读取对齐和缓冲区切分，未对齐的旁路区间可能重复读取同一扇区。

三种策略在 2025 年初赛 RISC-V64 官方原盘 `basic-musl` 上均为 **102/102**，32 个用例全部满分并正常关机。三种策略均通过文件 `mmap`/VMA 和 ELF 按需加载回归。默认策略还通过 basic ABI、fork COW、easy-fs 和宿主 EXT4 回归。宿主 EXT4 测试也覆盖自适应策略的碎片读、分句柄扫描识别及重复命中。

### 策略对比

在 QEMU 8.2.2、单核、128 MiB 内存下，每组负载使用同一份 32 MiB EXT4 镜像，每种策略独立启动三次，取中位数。混合负载先读一遍热点文件，再交替执行三轮 1,024 页顺序扫描与热点乱序访问。`reuse` 从冷缓存连续读同一段 128 页三次。`revisit` 先扫过另外 256 页填充缓存，再连续读同一段 128 页三次。BusyBox 取自 2025 年初赛 RISC-V64 官方盘的 `/musl/busybox`，测量冷页缓存下第一次执行 `true` 和后续七次启动。以下扇区数只统计测量区间。

| 负载 | 无缓存扇区 | LRU 扇区 | 自适应扇区 | LRU 热点未命中 | 自适应热点未命中 |
|---|---:|---:|---:|---:|---:|
| 32 页热点与顺序扫描 | 25,600 | 25,600 | 21,616 | 96 | 0 |
| 128 页热点与顺序扫描 | 28,672 | 28,672 | 23,920 | 384 | 0 |
| 单次顺序扫描 | 8,192 | 8,192 | 8,192 | — | — |
| `reuse` | 3,072 | 1,024 | 1,024 | — | — |
| `revisit` | 5,120 | 3,072 | 4,040 | — | — |
| BusyBox 启动八次 | 1,192 | 239 | 239 | — | — |

六组负载的三轮扇区和命中计数完全一致。两组混合负载中，自适应策略分别少读 15.6% 和 16.6% 的扇区，QEMU 客体耗时中位数由 1,760 和 2,050 ms 降至 1,386 和 1,631 ms。`revisit` 的自适应策略在第三遍已全部命中，前两遍的旁路仍使总扇区读取比 LRU 高 31.5%。BusyBox 上两种缓存策略都读 239 个扇区。考虑到满缓存后短扫描的重读成本，默认使用 LRU。本轮测量覆盖 BusyBox 启动，编译工作负载尚未测量。

在 Linux 环境下，从仓库根目录复跑。把 `official_image` 指向官方原盘，`debugfs` 以只读模式取出 BusyBox。每个场景第一次生成镜像，剩余轮次以 `-snapshot` 复用它。

```bash
official_image=/path/to/sdcard-rv.img
experiment=$(mktemp -d)
debugfs -R "dump /musl/busybox $experiment/busybox" "$official_image"
(cd user && cargo build --release --bin page_cache_probe --bin oscomp_runner)
export USER_BIN_DIR="$PWD/user/target/riscv64gc-unknown-none-elf/release"
export PAGE_CACHE_BUSYBOX="$experiment/busybox" QEMU_MEM=128M
for policy in none lru adaptive; do
    make all PAGE_CACHE_POLICY="$policy" KERNEL_OUT="$experiment/$policy"
done
for scenario in mixed32 mixed128 sequential reuse revisit busybox; do
    for trial in 1 2 3; do
        for policy in none lru adaptive; do
            PAGE_CACHE_IMAGE="$experiment/$scenario.img" PAGE_CACHE_POLICY="$policy" \
                KERNEL="$experiment/$policy" tools/oscomp/test_page_cache_policy.sh "$scenario" \
                "$experiment/$scenario-$policy-$trial.log"
        done
    done
done
```

## 2026-11-24 PR-09 fork 写时复制

`fork` 共享已驻留的私有用户页，覆盖用户栈、信号页、ELF 和匿名映射。父子任一侧写入时都会检查原映射权限。仍有其他引用的帧会复制，只剩当前映射引用的帧直接恢复可写。共享映射继续保持共享。子进程单独分配内核栈并复制 trap context。当前支持只有一个存活线程的进程执行 `fork`。

`tools/oscomp/test_fork_cow.sh` 覆盖父子隔离、`mprotect`、内核写回与信号帧、`MAP_SHARED`、子进程回收和内存不足回滚。在 128 MiB QEMU 中，父进程驻留 40 MiB 后可完成 `fork→exec`。PR-08 在同一压力样本上复制页时耗尽物理帧。

2025 年初赛 RISC-V64 官方原盘 `basic-musl` 为 **102/102**，32 个用例全部满分并正常关机。basic ABI、ELF 按需加载、文件 mmap/VMA、EXT4 页缓存和 easy-fs 文件读写回归通过。

### fork→exec 对比

用 QEMU 8.2.2、单核和 256 MiB，父进程预先写入 40 MiB 私有页，子进程立即执行另一份 ELF。两版各跑五轮，取中位数。延迟从 `fork` 入口计至新程序的第一次系统调用。峰值为这个区间内相对 `fork` 前新增的最多物理帧，包含页表、内核栈和地址空间所用帧。

| 指标 | PR-08 `9f623f51`（已加同一套计数器） | PR-09 |
|---|---:|---:|
| 复制物理页 | 10,504 | 1 |
| 额外峰值物理帧 | 10,540（41.17 MiB） | 277（1.08 MiB） |
| `fork` 耗时 | 51.006 ms | 6.770 ms |
| `fork→exec` 后首次系统调用 | 152.999 ms | 12.157 ms |

在 PR-09 工作区用相同的测试脚本跑两版。日志中只取 `path=fork-cow-leaf` 的记录。

```bash
experiment=$(mktemp -d)
git worktree add --detach "$experiment/eager-src" 9f623f51
make -C "$experiment/eager-src" all ELF_STARTUP_STATS=1 KERNEL_OUT="$experiment/eager"
make all ELF_STARTUP_STATS=1 KERNEL_OUT="$experiment/cow"
for trial in 1 2 3 4 5; do
    for mode in eager cow; do
        log="$experiment/$mode-$trial.log"
        FORK_BENCH_ONLY=1 QEMU_MEM=256M KERNEL="$experiment/$mode" tools/oscomp/test_fork_cow.sh "$log"
        grep -a '^\[fork-cow\] path=fork-cow-leaf ' "$log"
    done
done
```

## 2026-11-24 PR-08 ELF 按需加载

EXT4 上的 `exec` 按区间读取 ELF 头和程序头，登记 `PT_LOAD` 的 VMA。缺页时，完整文件页与普通 `read/readv`、文件 `mmap` 共用页缓存。私有写入触发 COW，部分页和 BSS 按需分配并补零。内嵌程序直接使用内核内的字节，easy-fs 与 Overlay 继续以文件快照为输入。

`tools/oscomp/test_elf_demand.sh` 覆盖非页对齐的段、同页相邻段、BSS、文件页写隔离、`mprotect` 后的缺页、内核向未驻留 BSS 写入，以及无效 ELF 和 `argv`。官方盘的 basic-musl ELF 含 `PT_INTERP`，保留直接进入 `e_entry` 的兼容路径。通用动态链接仍待适配。

2025 年初赛 RISC-V64 官方原盘 `basic-musl` 为 **102/102**，32 个用例全部满分并正常关机。basic ABI、文件 mmap/VMA、EXT4 页缓存和 easy-fs 文件读写回归通过。

### 冷启动对比

QEMU 8.2.2、单核、256 MiB RAM。合成 ELF 含一个 8 MiB 的冷文件段，其内存范围为 32 MiB，程序只执行首个代码页。两版各从冷盘启动五次，表中取中位数。计数从记录 exec 路径后开始，截止新程序发起第一次系统调用，包含首次缺页。帧分配统计成功分配次数，扇区读取计入 EXT4 数据和元数据。计数器为单核测试中的全局计数，耗时为客体墙钟时间。

| 截止第一次用户系统调用 | PR-07 装载器 `0eb2d7f1`（已加同一套计数器） | PR-08 按需装载 |
|---|---:|---:|
| 物理帧分配次数 | 8,475 | 268 |
| 512 字节扇区读取 | 16,412 | 28 |
| 耗时 | 2,124.110 ms | 3.326 ms |

在 PR-08 工作区用相同的测试脚本跑两版。日志中只取 `path=elf-startup` 的记录。

```bash
experiment=$(mktemp -d)
git worktree add --detach "$experiment/eager-src" 0eb2d7f1
make -C "$experiment/eager-src" all ELF_STARTUP_STATS=1 KERNEL_OUT="$experiment/eager"
make all ELF_STARTUP_STATS=1 KERNEL_OUT="$experiment/demand"
for trial in 1 2 3 4 5; do
    for mode in eager demand; do
        log="$experiment/$mode-$trial.log"
        ELF_STARTUP_ONLY=1 KERNEL="$experiment/$mode" tools/oscomp/test_elf_demand.sh "$log"
        grep -a '^\[elf-startup\] path=elf-startup ' "$log"
    done
done
```

## 2026-11-23 PR-07 文件系统与进程 ABI

2025 年初赛 RISC-V64 官方原盘的 `basic-musl` 在 QEMU 8.2.2、单核和 1 GiB 内存下达到 **102/102**，32 个用例全部满分并正常关机。`clone` 采用调用者指定的子栈，`mkdir` 可以创建并打开目录，`unlink` 按当前工作目录或目录 fd 解析路径。

`tools/oscomp/test_basic_abi.sh` 用独立 EXT4 镜像验证 `write/writev` 跨 1 MiB 上限短写、上限后的 `EFBIG`、目录创建和枚举、删除后继续使用已打开的文件、只读 EXT4 目录项遮蔽、异常路径和目录删除后重建。进程 `clone` 的子栈用独立用例校验。

## 2026-11-23 PR-06 统一文件页缓存

EXT4 普通 `read/readv` 和文件 `mmap` 缺页按文件系统实例、inode 和页号查找同一帧。LRU 缓存最多持有 256 个 4 KiB 页，帧分配遇到内存压力时可回收空闲缓存页。活动映射持有独立引用，缓存满且所有页仍被引用时，新读入的页按需使用，不留在缓存中。冷读以 4 KiB 填充缓存，ELF 装载仍走原有读取路径。

`tools/oscomp/test_ext4.sh` 在宿主侧对同一个 8 MiB 文件计数，扇区为 512 字节。

| 验证 | PR-05 `5f8737d8` | PR-06 |
|---|---:|---:|
| 首次读 32 KiB，分 8 次请求 | 64 次扇区读取 | 64 次扇区读取 |
| 回到起点再次读 4 KiB | 8 次扇区读取 | 0 次扇区读取 |

普通读取后取得映射页、取得映射页后普通读取，两条路径的第二步均为 0 次扇区读取。宿主测试还覆盖跨文件句柄复用、文件系统实例隔离、LRU 淘汰、满容量旁路及帧回收。

QEMU 下 `tools/oscomp/test_mmap_vma.sh` 验证了私有映射首次写入时复制页面，并覆盖权限变更、子进程写入、`read` 和 `pipe` 写回用户缓冲区、线程退出清除 TID。写入后同文件的其他映射和普通读取仍返回原数据。256 MiB 下的 129 MiB 稀疏文件区间读取也通过。

QEMU 8.2.2 上连续两次运行 2025 年初赛官方 RISC-V64 原盘 `basic-musl`，均为 **97/102**，32 个用例均执行，29 个用例满分并正常关机。失分项仍为 `clone`、`mkdir` 和 `unlink`。

## 2026-11-23 PR-05 文件映射与 VMA

QEMU 8.2.2 上用最终内核连续运行两次官方 RISC-V64 `basic-musl`，均为 **97/102**，29 个用例满分，32 个用例均执行并正常关机。`mmap` 从 2/3 升至 3/3，剩余失分来自 `clone`、`mkdir` 和 `unlink`。

在 Linux 或 WSL2 上用 `tools/oscomp/test_mmap_vma.sh` 可构建并启动专用 EXT4 测试盘。它覆盖六参数 `mmap`、非零文件偏移、关闭 fd 后访问、私有隔离、共享 overlay 与普通 I/O 一致、局部 `munmap`、局部 `mprotect`、固定地址替换，以及文件末页补零和跨 EOF 整页的 `SIGBUS`。测试同时检查 `waitpid` 的信号状态和共享缓冲区重叠时的文件读写。 [mmap(2)](https://man7.org/linux/man-pages/man2/mmap.2.html) · [mprotect(2)](https://man7.org/linux/man-pages/man2/mprotect.2.html)

当前可写文件共享映射支持内存 overlay。私有页在缺页时读取到独立帧。overlay 存在活动映射时，缩短文件或以 `O_TRUNC` 打开返回 `EBUSY`。`munmap` 和 `MAP_FIXED` 操作动态 VMA，ELF 装载段和任务资源由各自的生命周期管理。

## 2026-11-23 PR-04 EXT4 区间读取

普通 `read` 和 `readv` 按当前文件偏移读取所需扇区。新版 `tools/oscomp/test_ext4.sh` 覆盖跨块、分段缓冲区、EOF 短读、稀疏洞、未写入 extent 和索引 extent 树。

| 验证 | M1 基线 `601a839b` | PR-04 |
|---|---|---|
| 宿主对同一个 8 MiB 文件连续读 8 次，每次 4 KiB | 131,072 次扇区读取 | 64 次扇区读取，数据一致 |
| QEMU 256 MiB，从 129 MiB 稀疏文件的 128 MiB 偏移执行 BusyBox `dd`，读取 4 KiB 并用 `cmp` 校验 | 内核尝试分配 129 MiB，触发 panic | 内容一致，正常关机 |
| 官方 RISC-V64 原盘 `basic-musl` | 96/102 | 96/102，32 个用例均执行并正常关机 |

## 2026-11-23 官方盘回归

基线 `44a89a39` 在原盘上复现了首进程启动 panic。更新到 `ac9e23c7` 后，用 QEMU 8.2.2、1 核和 1 GiB RAM 连续运行三次。每次执行原盘 `/musl/basic_testcode.sh` 的 32 个用例，均为 **96/102**，28 个用例满分，QEMU 正常退出。三轮 QEMU 墙钟耗时依次为 4、5、4 秒。

- 镜像为 2025 初赛发布的 `sdcard-rv.img.xz` 解压所得，原盘 SHA-256 为 `95973543db6b84a9a5e70f30da466ce292867aff5b689fb14c88dc9406e378b8`。 [发布页](https://github.com/oscomp/testsuits-for-oskernel/releases/tag/pre-20250615)
- 判分使用同一发布版本的测试套件 `4b40ee92e8b594fb20e3957ebc2f79ad43aca538`。内核 SHA-256 为 `057544f007f71b18f98f9e5e3a0c3d6bb0693b1ab68b47a6a6e89535b952749f`。
- 官方盘跨块组文件 `/glibc/basic/brk` 的 56,472 字节与宿主 `debugfs` 结果逐字节一致。合成镜像中的三个文件分别验证了稀疏洞、未写入 extent 和带索引的 extent 树，读取结果均与预期一致。
- 四项失分为 `clone` 1/4、`mkdir` 2/3、`mmap` 2/3、`unlink` 1/2。三次 `pipe` 均得满分，历史偶发失败仍需观察。

以上是本地 QEMU 对历史官方盘的回归。复跑命令见 [官方盘回归](oscomp-evaluation.md#官方盘回归)。

## 2026-11-22 本地模拟盘记录

Martyr 的内核基于 `3bf874f` 加未提交的稀疏文件修复，以下分数来自平铺布局的本地模拟盘。

- 「Martyr 实测」是 Martyr 用官方判分方式跑出来的，可信。
- 「wonder-world 记录」来自 [适配改动说明](ADAPTATION_NOTES.md) 第 17 章，是 wonder-world 在自己机器上的结果，**没有经 Martyr 复现**。而且那些数字多数只表示「脚本跑到了 END」，不等于得分，不要当成绩用。

## 总表

| 组 | Martyr 实测 | wonder-world 记录（未复现） | 测试盘脚本 |
|---|---|---|---|
| basic | **96/102**（28/32 个用例满分，仅限本地模拟盘） | 称 32/32 | 有，全套 |
| lua | 未测 | 称 9/9 | `build_lua.sh`（已写，没跑过） |
| busybox | 未测 | 55 条命令 52 条成功 | 只有 `build_busybox.sh`（构建二进制），缺组盘脚本 |
| libc-test | 未测 | 94 个用例，88 Pass | `build_libctest.sh`（旧脚本，路径没对齐 `env.sh`） |
| libcbench | 未测 | 27 个 benchmark，256M 内存 | 缺 |
| lmbench | 未测 | 跑完，约 17 项数字，需 1G 内存 | `build_lmbench.sh`（旧脚本，同上） |
| unixbench | 未测 | 19 行结果 | 缺 |
| iozone | 未测 | 子测试全部执行 | 缺 |
| cyclictest | 未测 | 称 4/4 | 缺 |
| ltp | 未测 | 162 个用例，TPASS 54~60，且有波动 | `build_ltp.sh`（旧脚本，同上） |
| iperf / netperf | 未测 | 脚本跑完，全部子测试失败 | `build_net.sh`（旧脚本，同上） |

「旧脚本」指 wonder-world 留下的 4 个脚本，仍引用 `$HOME/rCore-Tutorial-v3` 和 `fsroot`，没有改成用 `tools/oscomp/env.sh`，Martyr 没试跑过。
「缺」指仓库里没有这个脚本，wonder-world 的文档提到过但没提交。

## basic 的 4 个失败（Martyr 实测，本地模拟盘）

| 用例 | 得分 | 现象 |
|---|---|---|
| clone | 1/4 | 子进程跳到栈地址执行，触发 `InstructionPageFault`（`sepc=0x105d60`），收到 SIGSEGV |
| mkdir | 2/3 | 输出了 `mkdir error` |
| mmap | 2/3 | 读出的 mmap 内容为空 |
| unlink | 1/2 | 输出了 `unlink error` |

原因还没查。

**结果不稳定**：同一张盘、同一个内核，连续 3 次运行中 2 次是 96/102，1 次是 92/102（`test_pipe` 得 0/4，断言 `[2, 3] (>=)` 失败）。`test_pipe` 存在偶发失败，原因未查，所以 96/102 只是典型值，不是确定值。

## 其他已知问题（Martyr 发现）

- **usertests 会让内核 panic**：在本地 easy-fs 镜像上跑 `usertests`，跑到 `race_adder_arg` 时内核 panic：
  `src/mm/page_table.rs:115 vpn 0xffe is mapped before mapping`。
  `0xffe000` 是信号 stub 页，疑似多线程场景下重复映射，没有进一步确认。此前的用例（`filetest_simple`、`cat`、`matrix`、`pipetest` 等）都正常退出。

## wonder-world 文档里提到的、仍未解决的限制

以下来自 wonder-world 记录，未重新验证：信号返回上下文被破坏会让 lmbench 的 `lat_ctx` 等子进程崩溃，单核调度下 cyclictest 压力阶段超时。内核仍缺少动态链接以及网卡驱动和 TCP/IP 栈。
