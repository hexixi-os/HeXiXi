# 测试状态

以下记录当前双架构官方原盘复测（QEMU 8.2.2，`--legacy-image`）与专项回归结果。

## 1. 官方 2025 初赛原盘

### `basic` 组

| 平台与模式 | `basic-musl` | `basic-glibc` | 备注 |
|---|---:|---:|---|
| RISC-V64 单核 | **102/102** | **102/102** | 正常退出，`--strict` 通过 |
| RISC-V64 双核（`SMP=1`，5 次） | 98/102 | 98/102 | 正常退出，失分集中在 `test_pipe` 串口交错 |
| LoongArch64 单核 | **102/102** | **102/102** | 正常退出，`--strict` 通过 |

### 完整 24 项矩阵（单核 1 GiB，每组 120 s，0 次内核 panic）

复跑命令见 [对接官方评测](oscomp-evaluation.md#3-完整-24-项评测矩阵)。

| 架构 | 正常退出 | 脚本异常 | 超时 | 备注 |
|---|---:|---:|---:|---|
| RISC-V64 | 18 | 0 | 6 | `lua` 双库 9/9；超时组为双库 `busybox` / `lmbench` / `ltp` |
| LoongArch64 | 17 | 0 | 7 | `lua` 双库 9/9；超时组另含 `libcbench-musl` |

### PR21 Pipe WaitQueue 原盘复测

RISC-V64 和 LoongArch64 均以单核、1 GiB 和每组 120 s 运行原盘。两架构的 `basic-musl` 与 `basic-glibc` 在改动版中均为 **102/102**。`lmbench`、`ltp` 的双架构双 libc 共 8 项在 `31aa0ce7` 和改动版上都达到超时上限，期间均无内核 panic。

### 网络测例（`iperf` / `netperf`）

- **Netperf 2.7.0**：使用独立 EXT4 盘（含按架构静态构建的 `netperf` / `netserver`），RISC-V64 与 LoongArch64 单核均 **5/5**（UDP/TCP stream、RR、CRR）。
- **原盘 iperf3**：已补齐 `TCP_INFO`，TCP/UDP 基础、5 路并行与反向模式在 RISC-V64 与 LoongArch64 双库（`iperf-musl` / `iperf-glibc`）原盘复测均稳定 **6/6**（120 s `--strict` 正常退出，0 次内核 panic）。
- **原盘 lmbench**：已补齐 `AF_UNIX`（`SOCK_STREAM` / `SOCK_DGRAM` / `SOCK_SEQPACKET`）、`socketpair`、`EPOLLET`、`epoll_pwait` 临时信号掩码、`SIGSEGV` 用户态恢复与 `mmap`/`munmap` 地址空间回收。全部 26 项原盘子测例及 `lat_unix` / `bw_unix` 在双架构双库下均 0 挂死、0 panic 跑通（`ENOUGH=5000` 固定采样下 RV64 双库约 71 s、LA64 双库约 68 s 完整退出；macOS Docker Desktop 虚拟化宿主因时钟抖动 >0.25% 触发 `compute_enough` 回退 `SHORT=1s`，默认 120 s 门限内原盘未改镜像部分完成）。

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
