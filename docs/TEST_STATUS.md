# 测试状态

最后更新：2026-11-22，测试人：Martyr。内核基于提交 `3bf874f`，加上 `fs/ext4.rs` 的稀疏文件修复（未提交）。

## 先看这个：模拟盘分数不等于评测机分数

下面的「Martyr 实测」都是在**本地模拟盘**（平铺布局，自建）上测的。Martyr 另外用官方 Docker 镜像生成了**真实官方盘**，并用当前内核按官方 QEMU 参数启动：

- 内核能识别 EXT4 并列出根目录（`glibc`、`lib`、`lib64`、`musl`）。
- 随后在 `os/src/task/mod.rs:190` panic（`Option::unwrap()` 为 `None`）：内核要从盘里读 `oscomp_runner` 或 `initproc`，官方盘根目录没有它们。**一个测试都没跑**。
- `glibc` 目录读出 `ino=131073 size=0`，疑似读第二个块组的 inode 有问题，未确认。
- glibc 那一套需要动态链接，内核不支持。

所以**真实官方盘上的得分目前是 0**。差异对照和生成方法见 [开发与测试指南](DEVELOPMENT.md) 第 6 节。

**怎么读这张表**：

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
- **EXT4 稀疏文件**：已修复，之前会导致 `exec` 读到被截断的 ELF 而 panic。
- **EXT4「未写入的 extent」**（fallocate 预分配）现在被当作真实数据读，可能读到旧内容。目前没遇到，只是代码上看到。
- **每次 `read()` 都把整个文件重新读一遍**（`Ext4File::read` → `read_file`），大文件会很慢。

## wonder-world 文档里提到的、仍未解决的限制

以下来自 wonder-world 记录，未验证：信号返回上下文被破坏会让 lmbench 的 `lat_ctx` 等子进程崩溃；单核调度下 cyclictest 压力阶段超时；没有动态链接（libc-test 动态部分被跳过）；`execve` 不支持 `#!` 解释器行；没有网卡和 TCP/IP 栈。
