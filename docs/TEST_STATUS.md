# 测试状态

以下记录双架构官方原盘复测与专项回归结果。原盘复测均使用 `--legacy-image`。

## 1. 官方 2025 初赛原盘

### `basic` 组

| 平台与模式 | `basic-musl` | `basic-glibc` | 备注 |
|---|---:|---:|---|
| RISC-V64 单核 | **102/102** | **102/102** | 正常退出，`--strict` 通过 |
| RISC-V64 双核（`SMP=1`，5 次） | 98/102 | 98/102 | 正常退出，失分集中在 `test_pipe` 串口交错 |
| LoongArch64 单核 | **102/102** | **102/102** | 正常退出，`--strict` 通过 |

### 完整 24 项矩阵基线（QEMU 8.2.2，单核 1 GiB，每组 120 s，0 次内核 panic）

复跑命令见 [对接官方评测](oscomp-evaluation.md#3-完整-24-项评测矩阵)。

| 架构 | 完成 | 脚本异常 | 超时 | 提前截断 | 备注 |
|---|---:|---:|---:|---:|---|
| RISC-V64 | **21** | 0 | 2 | 1 | `basic` 双库 102/102、`lua` 双库 9/9、`busybox` 双库 52/55、`iperf` 双库 6/6。`lmbench` 双库超时，`ltp-glibc` 清单被提前截断 |
| LoongArch64 | **21** | 0 | 3 | 0 | 双库 `ltp` 跑到清单末尾。超时组为双库 `lmbench` 与触发 QEMU 8.2.2 TCG `ll.w` 缺陷的 `libcbench-musl` |

### PR23 和 PR24 原盘验收复测

使用 macOS 原生 QEMU 11.1.3，以单核、QEMU 内存配置 1 GiB 和 `--legacy-image --timeout 120 --strict` 复测 `basic busybox libcbench lmbench iperf`。每个架构的 musl 和 glibc 共 10 组均正常退出，0 次内核 panic。`basic` 四组均为 **102/102**，`busybox` 四组均为 **52/55**，`iperf` 四组均为 **6/6**。

| 平台 | `lmbench-musl` | `lmbench-glibc` |
|---|---:|---:|
| RISC-V64 | 64.94 s | 55.07 s |
| LoongArch64 | 58.62 s | 53.61 s |

LoongArch64 `libcbench-musl` 在 QEMU 11.1.3 下耗时 **1.43 s**，同一份未改动的基线内核在 QEMU 8.2.2 下仍复现卡在 `b_malloc_thread_stress`。两版 QEMU 的 LoongArch `ll.w` 翻译在保存链接地址与写回目标寄存器的顺序上存在差异。[QEMU 8.2.2 源码](https://github.com/qemu/qemu/blob/v8.2.2/target/loongarch/insn_trans/trans_atomic.c.inc#L5-L15) · [QEMU 11.1.3 源码](https://github.com/qemu/qemu/blob/v11.1.3/target/loongarch/tcg/insn_trans/trans_atomic.c.inc#L25-L28)

旧版原盘的 `hello` 引用了镜像构建机的路径。启动器识别该占位文件，在内存 Overlay 中提供可直接执行的 `hello` 和可用的 `/bin/sh`、`cp`，随后运行原盘脚本的全部 24 条 `lmbench_all` 命令。启动器复用一次 `lmbench` 自动校准的结果，校准回退到 1 s 时改用 5 ms 功能回归采样并记录在串口日志中。本次四组均使用了 5 ms，耗时用于功能验收。

网络 ABI 和 VirtIO TCP 回显在 RISC-V64 单核及双核、LoongArch64 单核共六组均通过。

### PR21 Pipe WaitQueue 原盘复测

RISC-V64 和 LoongArch64 均以单核、1 GiB 和每组 120 s 运行原盘。两架构的 `basic-musl` 与 `basic-glibc` 在改动版中均为 **102/102**。

### PR25 LTP 定向攻克与原盘复测

以 `379ec5bf` 为基线。两架构均以单核、1 GiB、QEMU 8.2.2 和 `--legacy-image --timeout 120 --strict` 运行 2025 初赛原盘：

1. **内核批量修复聚类**：
   - 定位与向量 I/O、零拷贝传输：`pread64`/`pwrite64`/`preadv`/`pwritev`/`preadv2`/`pwritev2`、`readv`/`writev` 边界校验、`sendfile`（含显式偏移指针读写校验）、`fallocate`（预分配、`KEEP_SIZE` 与 `PUNCH_HOLE`）、`lseek` 负偏移 `EINVAL` 与管道/套接字 `ESPIPE`。
   - 内存管理与资源上限：`mremap`（原地扩缩、`MREMAP_MAYMOVE`、`MREMAP_FIXED`、0 长度共享映射）、`madvise` 映射空洞/参数校验、`getrlimit`/`setrlimit`/`prlimit64` 及 `RLIMIT_CPU` 软硬时限信号（`SIGXCPU`/`SIGKILL`）。
   - 信号、间隔定时器与健壮锁：`rt_sigpending`、`rt_sigsuspend`、`getitimer`/`setitimer`（含旧值写回与周期重装）、`set_robust_list`/`get_robust_list` 及线程退出 `FUTEX_OWNER_DIED` 唤醒、`IDLE_PID` 默认信号保护。
   - 进程凭证、DAC 权限与虚拟文件系统：完整 UID/GID/附属组与 `capget`/`personality` 系统调用、`faccessat`/`openat`/`chmod`/`fchmod`/`fchmodat`/`fchmodat2`/`chown`/`fchown`/`fchownat`/`utimensat` 权限及 sticky 目录校验、`O_PATH` 套接字 `EBADF`、`/proc/cmdline`、`/proc/sys/{kernel,fs,vm}` 与 `/proc/<pid>/{maps,mounts,fd,task}`，并将 EXT4 目录缓存上限提升至 512 KiB 以消除 `ltp/testcases/bin` 大目录重复读盘。
2. **原盘 LTP 执行器（`oscomp_runner`）**：补齐 `/etc/{passwd,group,hosts,protocols}`（含 `nobody` 账户）与 `/bin/sh` 环境；针对初赛原盘 `ltp_testcode.sh` 未过滤 `cgroup_fj_proc` 等后台辅助程序导致的永久挂起，流式解析原盘自带 `/$libc/ltp/runtest/syscalls` 清单并附带单测例进程组 `SIGTERM→SIGKILL` 限时回收，同时保留自定义 `ltp_testcode.sh` 子集盘的直接执行兼容。

专项子集对照（按用例退出码 0 计通过，RV64/LA64 双库一致）：

| 子集 | `379ec5bf` 基线 | PR25 | 备注 |
|---|---:|---:|---|
| `pread` / `pwrite` / `preadv` / `pwritev` 及 v2 | 0/30 | **28/30** | 仅 `preadv203`、`preadv203_64` 需块设备 loop 挂载 |
| `getrlimit02`、`setrlimit03` 至 `setrlimit06` | 1/5 | **5/5** | 含 `RLIMIT_CPU` 触发 `SIGXCPU`/`SIGKILL` |
| `rt_sigsuspend01`、`sigpending02`、`sigsuspend01` | 0/3 | **3/3** | — |
| 合计（每组） | 1/38 | **36/38** | |

原盘移除执行器内部 `LTP_GROUP_BUDGET_MS` 截断（改由宿主机统一控制整组超时，客机仅控制单用例超时）并修复 `mremap(MREMAP_FIXED)` 目标页表预分配与回滚后，使用 macOS 原生 QEMU 11.1.3（单核、1 GiB、`--legacy-image --timeout 120 --strict`）复测，**双架构四组 LTP 全部在 120 s 内完整跑到 1,397 项系统调用清单末尾，0 次内核 panic**，`basic` 双架构双库保持 **102/102**：

| 平台 / libc | 状态 / 耗时 | 运行用例数 | 退出码 0（另 `TCONF` 跳过） | 官方判分 `Summary.passed` | `TPASS` 行数 |
|---|---|---:|---:|---:|---:|
| RV64 `ltp-musl` | **complete** / 69.3 s | 1,397 | **488** (+303) | **2,534** | 3,312 |
| RV64 `ltp-glibc` | **complete** / 73.8 s | 1,397 | **495** (+297) | **2,577** | 3,385 |
| LA64 `ltp-musl` | **complete** / 53.3 s | 1,397 | **488** (+303) | **2,528** | 3,308 |
| LA64 `ltp-glibc` | **complete** / 47.6 s | 1,397 | **496** (+297) | **2,584** | 3,395 |

> 注：此前在容器内 QEMU 8.2.2 下，RV64 `ltp-glibc` 曾因 102 s 内部预算提前停在 `futex_waitv02`（1,368/1,397 项）；移除内部截断后，容器 QEMU 8.2.2 因仿真速度较慢会触发 120 s 宿主机超时，原生 QEMU 11.1.3 下四组均可在 48~74 s 内完整跑完。

`make all`、`make all SMP=1` 和 `make kernel-la` 均零告警通过。单核官方 `basic` 双架构双库均为 **102/102**，RV64 双核双库为 98/102（与基线一致），`busybox` 双架构双库保持 **52/55**。`test_fork_cow.sh`、`test_mmap_vma.sh`、`test_page_cache_policy.sh`、`test_smp.sh`、`test_overlay_fs.sh`、`ext4_probe`、`easy-fs` 宿主测试及双架构 VFS、网络、Pipe 等待、ELF 动态/按需加载和 EasyFS 持久化等全套专项回归全部通过。

### PR-26 全量原盘判分复测

以 `bca16c8f`（PR-25）为基线，使用 2025 初赛原盘、QEMU 11.1.3、单核、QEMU 内存参数 1 GiB、`--legacy-image` 和每组 120 s 宿主机上限。每个架构各启动 24 组，每组独立启动 QEMU。基线 RV64 glibc LTP 在并发采样时触发过超时，表中采用同一基线内核随后完成的 76.11 s 复测。测试套件、判分器版本及启动命令见 [对接官方评测](oscomp-evaluation.md#3-完整-24-项评测矩阵)。

#### 24 组矩阵对照

基线双架构各完成 24/24 组。PR-26 双架构各完成 22/24 组，超时均为 UnixBench，双架构全矩阵内核 panic 为 0。下表为基线 → PR-26。带公开判分器的组列得分，`iozone` 为 20 项判分之和并保留两位小数。其余组列用例成功数、有效测项数或耗时。`ltp` 两个数字依次为退出码 0 的用例数和 `Summary.passed` 总和。cyclictest 的压力源状态指 `hackbench` 的启动与退出结果。

| 测试组 | RV64 musl | RV64 glibc | LA64 musl | LA64 glibc |
|---|---:|---:|---:|---:|
| `basic` | 102/102 → 102/102 | 102/102 → 102/102 | 102/102 → 102/102 | 102/102 → 102/102 |
| `busybox` | 52/55 → 54/55 | 52/55 → 54/55 | 52/55 → 54/55 | 52/55 → 54/55 |
| `lua` | 9/9 → 9/9 | 9/9 → 9/9 | 9/9 → 9/9 | 9/9 → 9/9 |
| `libctest` | 106/220 → 217/220 | 85/220 → 179/220 | 0/220 → 217/220 | 85/220 → 179/220 |
| `libcbench` | 27 项 → 27 项 | 27 项 → 27 项 | 27 项 → 27 项 | 27 项 → 27 项 |
| `lmbench` | 65.62 s → 67.49 s | 50.31 s → 50.07 s | 56.98 s → 62.72 s | 57.81 s → 55.65 s |
| `unixbench` | 0/27 项 → 8/27 项（超时） | 0/27 项 → 8/27 项（超时） | 0/27 项 → 8/27 项（超时） | 0/27 项 → 8/27 项（超时） |
| `iozone` | 0.00 → 23.81 | 0.00 → 29.63 | 0.00 → 39.26 | 0.00 → 39.57 |
| `cyclictest` | 4/4，压力源失败 → 4/4，压力源退出 | 4/4，压力源失败 → 4/4，压力源退出 | 0/4，压力源失败 → 0/4，压力源退出 | 0/4，压力源失败 → 4/4，压力源退出 |
| `ltp` | 485 / 2,530 → 699 / 3,605 | 495 / 2,579 → 704 / 3,599 | 487 / 2,527 → 704 / 3,616 | 494 / 2,579 → 707 / 3,731 |
| `iperf` | 6/6 → 5/6 | 6/6 → 6/6 | 5/6 → 5/6 | 6/6 → 6/6 |
| `netperf` | 0/5 → 5/5 | 0/5 → 5/5 | 0/5 → 5/5 | 0/5 → 5/5 |

#### LTP 二轮实测

四组最终内核均在 120 s 内执行到原盘系统调用清单末尾，0 次内核 panic。基线每组执行 1,397 项。PR-26 额外过滤 `dirtyc0w`、`dirtypipe`、`leapsec01`，每组实际执行 1,394 项。执行器改用单调时钟限制单项耗时，默认从 450 ms 调整为 320 ms，定时器、信号等需要等待的指定用例使用 1,250 ms。发送 `SIGTERM` 后的宽限从 80 ms 调整为 50 ms。

| 平台 / libc | 退出码 0，基线 → PR-26 | `Summary.passed`，基线 → PR-26 | PR-26 `TCONF`（退出码 32） | PR-26 耗时 |
|---|---:|---:|---:|---:|
| RV64 musl | 485 → **699** | 2,530 → **3,605** | 307 | 93.25 s |
| RV64 glibc | 495 → **704** | 2,579 → **3,599** | 302 | 112.82 s |
| LA64 musl | 487 → **704** | 2,527 → **3,616** | 307 | 62.95 s |
| LA64 glibc | 494 → **707** | 2,579 → **3,731** | 303 | 67.11 s |

下表合计双架构双库的退出码 0 用例。统计范围为前后两次均执行且均未返回 `TCONF` 的案例，可直接比较同一批用例。

| 用例聚类 | 基线 → PR-26 | 可比较用例数 |
|---|---:|---:|
| `eventfd`、`timerfd`、`signalfd`、`memfd_create` | 0 → 40 | 52 |
| `fcntl`、`flock` | 80 → 220 | 284 |
| 硬链接、`copy_file_range`、`splice`、`vmsplice`、`tee` | 0 → 46 | 88 |
| SysV 共享内存、信号量、消息队列 | 0 → 133 | 184 |
| `getpriority`、`setpriority`、`prctl` | 0 → 8 | 40 |

#### 失败项与环境限制

- BusyBox 原盘引导环境提供可执行的 `ls` 和 `/dev/rtc`，`which ls`、`hwclock` 恢复。脚本的 54 个不同命令均输出成功，包括启动子进程后执行的 `kill $!`。固定版本判分器还要求原盘命令表中缺失的 `kill 10`，因此四组官方分均为 54/55。
- musl `libctest` 剩余静态 `crypt`、静态 `pleval` 和动态 `crypt`。glibc 剩余失分主要涉及 locale、宽字符、正则、线程取消和 DNS 解析。逐项结果可从各组的 `score.json` 查询。
- LA64 原盘 musl 的 `sched_getparam`、`sched_getscheduler` 和 `sched_setscheduler` 均直接返回 `ENOSYS`，串口插桩确认 `sched_getparam` 未到达内核。该组的四个 `cyclictest` 子项仍失败，`hackbench` 可正常启动并退出。调度 API 保存进程级策略和优先级，内核仍使用 FIFO 就绪队列。
- 基线的 UnixBench 脚本可退出，但待测程序因解释器路径缺失而无法启动。原盘 UnixBench 的 27 项依次运行，脚本传入的采样时长合计 390 s。四份原盘用户态目录均缺少 `sort.src`，影响三个 SHELL 子项。用 PR-26 首轮内核在补入官方 `sort.src` 的镜像副本上诊断，四组都在 480 s 内输出前 26 项。最终内核按原脚本参数单独运行 `./syscall 10 exec`，双架构双库均取得最后一项数据。
- 双架构 musl 的 `iperf BASIC_UDP` 在矩阵中出现客户端连接拒绝。原脚本在后台启动服务端后立即连接，使用同一最终内核单独复测时，两组均为 6/6。

#### 构建与专项回归

`make all`、`make all SMP=1`、`make kernel-la` 均通过，三个日志各 0 条告警。RV64 双核原盘 `basic` 双库均为 98/102，失分集中在 `test_pipe` 输出，单核双架构双库均为 102/102。

`test_vfs_abi.sh`、`test_pipe_wait.sh` 在 RV64 单核、双核和 LA64 单核通过。`test_net_abi.sh` 在双架构单核通过。`test_elf_demand.sh`、`test_elf_dynamic.sh`、`test_overlay_fs.sh`、`test_pthread_abi.sh`、`test_smp.sh` 及宿主机矩阵解析单测（9/9）通过。新增检查覆盖普通模式解释器缺失、放弃 UID 0 时清除 capability、`splice` 背压、timerfd/signalfd 唤醒、SysV 映射替换及文件锁释放。

### 网络测例（`iperf` / `netperf`）

- **Netperf 2.7.0**：使用独立 EXT4 盘（含按架构静态构建的 `netperf` / `netserver`），RISC-V64 与 LoongArch64 单核均 **5/5**（UDP/TCP stream、RR、CRR）。
- **原盘 iperf3**：已补齐 `TCP_INFO`，TCP/UDP 基础、5 路并行与反向模式在 RISC-V64 与 LoongArch64 双库（`iperf-musl` / `iperf-glibc`）原盘复测均稳定 **6/6**（120 s `--strict` 正常退出，0 次内核 panic）。
- **原盘 lmbench**：已补齐 `AF_UNIX`（`SOCK_STREAM` / `SOCK_DGRAM` / `SOCK_SEQPACKET`）、`socketpair`、`EPOLLET`、`epoll_pwait` 临时信号掩码、`SIGSEGV` 用户态恢复与 `mmap`/`munmap` 地址空间回收。双架构双库原盘耗时与校准配置见上方 PR23 和 PR24 复测记录。

## 2. 专项回归覆盖

| 专项 | 覆盖范围 |
|---|---|
| `test_exec_entry.sh` | 双架构（含 RV64 双核）：内嵌 runner 组选择、ELF 初始栈/寄存器、RISC-V `clone` 子栈指针 |
| `test_vfs_abi.sh` | 双架构（含 RV64 双核）：符号链接、UTF-8 路径、`mount`/`umount2`/`statfs`、`/proc/self`、`/dev/{random,urandom,zero}`、`/lib64`、`/dev/shm` 跨进程共享 `futex` 与 `clear_child_tid` |
| `test_net_abi.sh` | 双架构（含 RV64 双核）：无网卡 IPv4 TCP/UDP 回环、`TCP_INFO`、`AF_UNIX`（流式/数据报/顺序包、路径与抽象命名空间、`socketpair`、`SO_PEERCRED`）、半关闭、非阻塞、`ppoll`/`epoll`（含 `EPOLLET` 与 `epoll_pwait` 临时信号掩码）及 `--virtio` 宿主↔客机 TCP 回显 |
| `test_persistent_fs.sh` | 双架构（含 RV64 双核，含 `PAGE_CACHE_POLICY=none`）：EasyFS 数据盘 4 次重启回归、`fsync`/`msync`、盘上 ELF 执行、空白/只读盘错误处理与只读根盘哈希校验 |
| `test_smp.sh` | RV64 双核：`getcpu`/`sched_getaffinity`、跨核 CoW、共享文件截断 `SIGBUS`、超容量跨核管道逐字节校验 |
| `test_pipe_wait.sh` | RV64 单核与双核、LoongArch64 单核均通过空管道等待、满管道释放空间、多读者唤醒、EOF、`EPIPE`、`SIGKILL`、线程组退出及 `SIGALRM`。原 `run_pipe_test` 在 RV64 单核与双核各通过 1,000 轮 |
| `test_loongarch_boot.sh` | LoongArch64 ELF 直启、VirtIO PCI 只读 EXT4 与 `clone` TLS/child TID |
| `test_overlay_fs.sh` | 768 MiB 下 100 MiB 顺序写、>2 GiB 稀疏写、预算回收、`MAP_SHARED`+`truncate`/`SIGBUS`、`O_TMPFILE`；下层 EXT4 只读不变 |
| `test_pthread_abi.sh` | 动态 musl / glibc 各 16×8 并发线程及动态 glibc BusyBox |
| `test_elf_dynamic.sh` / `test_elf_demand.sh` | musl/glibc PIE 与高地址非 PIE `PT_INTERP` 动态装载、按需加载、共享页缓存与 `mprotect` |
| `test_fork_cow.sh` / `test_mmap_vma.sh` / `test_basic_abi.sh` | `fork` CoW、六参数 `mmap`/VMA 拆分、`MAP_FIXED`、跨 EOF `SIGBUS` 与基础 ABI |
| `test_ext4.sh` & EasyFS 宿主测试 | EXT4 宿主测例 + `easy-fs` 宿主测试 9/9（空间统计、满盘回滚、重命名、截断与盘间缓存隔离） |
| easy-fs `usertests` | RV64 单核/双核：29 正常用例 + 8 预期失败用例 + 1,000 轮管道测试全部通过 |

## 3. 关键性能基准（RISC-V64 单核）

| 场景 | 基线 | 当前 |
|---|---|---|
| ELF 按需冷启动（8 MiB 文件 / 32 MiB 内存，256 MiB RAM，5 次中位数） | 8,475 帧 / 16,412 扇区 / 2,124.1 ms | **268 帧 / 28 扇区 / 3.3 ms** |
| `fork→exec` CoW（40 MiB 私有驻留页，256 MiB RAM，5 次中位数） | 复制 10,504 页 / 峰值 +41.2 MiB / 153.0 ms | **复制 1 页 / 峰值 +1.1 MiB / 12.2 ms** |

### 页缓存策略对照（128 MiB 单核，32 MiB EXT4，3 次中位数，512 B 扇区读）

| 负载 | `none` | `lru`（默认） | `adaptive` |
|---|---:|---:|---:|
| 32 页热点 + 顺序扫描 | 25,600 | 25,600（热点未命中 96） | **21,616**（未命中 0） |
| 128 页热点 + 顺序扫描 | 28,672 | 28,672（热点未命中 384） | **23,920**（未命中 0） |
| `reuse`（连续重读 128 页×3） | 3,072 | **1,024** | **1,024** |
| `revisit`（满缓存后重读 128 页×3） | 5,120 | **3,072** | 4,040 |
| BusyBox 启动 8 次 | 1,192 | **239** | **239** |

> 默认采用 `lru`；可通过 `PAGE_CACHE_POLICY=adaptive|none` 切换，`/proc/page_cache` 提供统计快照。

## 4. Pipe WaitQueue 调度开销

QEMU 8.2.2，RISC-V64，256 MiB。每种配置运行 5 次，表中为中位数。基线使用 `31aa0ce7` 加相同计数器，工作负载为 12 次间隔 15 ms 的单字节写入。[复现命令](DEVELOPMENT.md#pipe-调度开销对照)

| 核数 | 任务派发次数，基线 → WaitQueue | 让出 CPU 后重新入队次数，基线 → WaitQueue |
|---|---:|---:|
| 单核 | 36 → 32 | 34 → 18 |
| 双核 | 207,792 → 43 | 207,790 → 29 |
