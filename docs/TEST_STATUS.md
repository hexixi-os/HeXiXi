# 测试状态

记录当前代码（RISC-V64 单核/双核 + LoongArch64 单核）在 2025 初赛官方原盘及专项回归上的最新实测结果（QEMU 8.2.2）。

## 1. 官方 2025 初赛原盘 `basic`

| 平台与模式 | `basic-musl` | `basic-glibc` | 备注 |
|---|---:|---:|---|
| RISC-V64 单核 | **102/102** | **102/102** | 32 用例全部满分；musl 正常退出，glibc 外层 BusyBox 在组结束后退出 139 |
| RISC-V64 双核（`SMP=1`，5 次） | 98–102/102 | 98–99/102 | 失分均来自 `test_pipe` 父子串口输出交错；组标记均完整 |
| LoongArch64 单核 | **102/102** | **102/102** | 32 用例全部满分；无依赖 PIE 直启 + `statx`；glibc 外层脚本结束退出 139 |

## 2. 专项回归（全部通过）

| 专项 | 覆盖范围 |
|---|---|
| `test_smp.sh` | RISC-V64 `-smp 2` 连续 12 次通过（双启动 hart 各 6 次），核对 `getcpu` / `sched_getaffinity`、跨核 CoW 与共享文件截断 `SIGBUS` |
| `test_loongarch_boot.sh` | LoongArch64 ELF 直启、VirtIO PCI 只读 EXT4、用户态执行与 `clone` TLS/child TID |
| `test_overlay_fs.sh` | 768 MiB 下 100 MiB 顺序写、>2 GiB 稀疏写、预算上限与回收、`MAP_SHARED` + `truncate`/`SIGBUS`、重命名与 `O_TMPFILE`；下层 EXT4 SHA-256 不变 |
| `test_pthread_abi.sh` | 动态 musl 1.2.0 / glibc 2.31 各 16×8 并发线程（mutex/condvar/TLS/TID/信号/futex/`exit_group`）及动态 glibc BusyBox |
| `test_elf_dynamic.sh` | musl PIE、glibc PIE 与高地址非 PIE 经 `PT_INTERP` 动态装载、堆扩缩与异常解释器处理 |
| `test_elf_demand.sh` | 按需 ELF 加载与共享页缓存，覆盖非页对齐段、BSS、`mprotect` 与异常 ELF |
| `test_fork_cow.sh` | `fork` 写时复制、父子隔离、`MAP_SHARED`、内存不足回滚与 128 MiB 下 40 MiB 驻留 `fork→exec` |
| `test_mmap_vma.sh` & `test_basic_abi.sh` | 六参数 `mmap`、局部 `munmap`/`mprotect`、`MAP_FIXED`、跨 EOF `SIGBUS` 与基础文件/目录/进程 ABI |
| `test_ext4.sh` | 宿主侧跨块组、稀疏洞、未写入 extent、索引 extent 树与页缓存复用/淘汰 |
| easy-fs `usertests` | 单核与双核均跑完 29 项正常用例、8 项预期失败用例及 1,000 轮管道测试 |

## 3. 关键性能基准

### ELF 按需装载冷启动（8 MiB 文件段 / 32 MiB 内存段，256 MiB 单核，5 次中位数）

| 指标（至首次用户系统调用） | 全量预加载基线 | 当前按需装载 |
|---|---:|---:|
| 物理帧分配次数 | 8,475 | **268** |
| EXT4 扇区读取（512 B） | 16,412 | **28** |
| 耗时 | 2,124.1 ms | **3.3 ms** |

### `fork→exec` 写时复制（40 MiB 私有驻留页，256 MiB 单核，5 次中位数）

| 指标 | 全量复制基线 | 当前 CoW |
|---|---:|---:|
| 复制物理页 | 10,504 | **1** |
| 额外峰值物理帧 | 10,540（41.2 MiB） | **277（1.1 MiB）** |
| `fork` / `fork→exec` 耗时 | 51.0 ms / 153.0 ms | **6.8 ms / 12.2 ms** |

### 页缓存策略对照（128 MiB 单核，32 MiB EXT4，3 次中位数）

| 负载 | `none` 扇区 | `lru`（默认）扇区 | `adaptive` 扇区 |
|---|---:|---:|---:|
| 32 页热点 + 顺序扫描 | 25,600 | 25,600（热点未命中 96） | **21,616**（未命中 0） |
| 128 页热点 + 顺序扫描 | 28,672 | 28,672（热点未命中 384） | **23,920**（未命中 0） |
| `reuse`（连续重读 128 页×3） | 3,072 | **1,024** | **1,024** |
| `revisit`（满缓存后重读 128 页×3） | 5,120 | **3,072** | 4,040 |
| BusyBox 启动 8 次 | 1,192 | **239** | **239** |

> 考虑满缓存后短扫描的重读成本，默认采用 `lru`；可通过 `PAGE_CACHE_POLICY=adaptive|none` 切换，`/proc/page_cache` 提供只读统计快照。

## 4. 官方 24 项矩阵基线与待解决问题

PR-11 基线（单核 1 GiB、每组 120 s，0 次内核 panic）全量实测摘要，复跑命令见 [对接官方评测](oscomp-evaluation.md#3-完整-24-项评测矩阵)：

| 状态 | 测试组 | 主要原因 / 观察 |
|---|---|---|
| 满分 | `basic`（musl/glibc 均 102/102） | glibc 外层 BusyBox 在结束标记后触发用户态取指缺页退出 139 |
| 部分运行 / 超时 | `busybox`、`lmbench`、`ltp`、`libcbench`、`unixbench`、`cyclictest` | 缺少部分工具/接口、动态库路径未部署或单核超时 |
| 0 分 / 不可用 | `lua`、`libctest`、`iozone`、`iperf`、`netperf` | 原盘脚本 shebang/路径缺失、动态链接器 `/lib` 缺失、无网卡与 TCP/IP 栈 |
