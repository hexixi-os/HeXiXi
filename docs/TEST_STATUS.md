# 测试状态

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
