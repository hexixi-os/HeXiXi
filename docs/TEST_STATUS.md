# 测试状态

最后更新：2026-11-23，测试人：Martyr。分支 `feat/official-disk`。

**怎么读**：

- 「Martyr 实测」是 Martyr 自己跑出来的，可信。「wonder-world 记录」来自 [ADAPTATION_NOTES.md](ADAPTATION_NOTES.md) 第 17 章，是他在自己机器上的结果，**没有经 Martyr 复现**，且多数只表示「脚本跑到了 END」，不等于得分。
- 实测分两种盘：**真实官方盘**（官方 Docker 镜像里跑官方 `make` 生成，4 GB，根目录是 `musl/ glibc/ lib/ lib64/`）和**本地模拟盘**（自建、平铺）。以真实官方盘为准，两者的区别和生成方法见 [DEVELOPMENT.md](DEVELOPMENT.md) 第 6 节。

## 真实官方盘上的实测（musl 部分）

运行方式：官方 QEMU 参数（1G 内存、单核、带网卡和第二块盘），内核从盘里找不到 `oscomp_runner` 时使用编进内核的那份，工作目录设为 `musl/`。**glibc 那一套没有跑**（需要动态链接，内核不支持）。

| 组 | 结果 | 说明 |
|---|---|---|
| basic | **96/102**（28/32 个用例满分） | 官方判分器；4 个失败见下 |
| lua | **9/9** | |
| busybox | 51 条成功，4 条失败 | |
| libctest | 107 个用例，98 个 `Pass!`，9 个没通过 | 失败例：`pthread_cancel`、`setjmp`、`socket` 等 |
| libcbench | 25 个 benchmark 有结果 | 没有标准判分口径，只数了有时间输出的 |
| lmbench | 11 项数字 | 能跑完，延迟和带宽各有输出 |
| cyclictest | 4 个用例全部 `fail`（`kill hackbench` 为 success） | |
| iperf | 全部 fail | `open /dev/urandom` 返回 `Operation not permitted`，且没有网络栈 |
| unixbench | **大量崩溃** | 反复出现 `InstructionPageFault sepc=0xfffffffffffffffe`，结果不完整 |
| iozone | **大量崩溃** | 同上 |
| ltp | **内核 panic，没有结果** | 一开始遍历 `ltp/testcases/bin/`（2822 个文件）时，读 extent 索引块报 `virtio IoError` |
| netperf | **没跑到** | 排在 ltp 之后，ltp 的 panic 使内核退出 |

**整轮没有走到正常关机**：ltp 处 panic 后 QEMU 退出，没有 `Idle process exit`。完整一轮约 20 多分钟，跑它时请给足超时（我们用 2400 秒）。

### basic 的 4 个失败

| 用例 | 得分 | 现象 |
|---|---|---|
| clone | 1/4 | 子进程触发非法指令/页错误，收到 SIGILL 或 SIGSEGV |
| mkdir | 2/3 | 输出了 `mkdir error` |
| mmap | 2/3 | 读出的 mmap 内容为空 |
| unlink | 1/2 | 输出了 `unlink error` |

原因都还没查。在本地模拟盘上同样是这 4 个失败，分数一致。另外在模拟盘上曾观察到 `test_pipe` 偶发失败（3 次运行中 1 次，得 0/4），在官方盘上只跑了 1 次，没遇到。

## wonder-world 记录对照（未复现）

| 组 | wonder-world 记录 | 与 Martyr 在官方盘上的实测 |
|---|---|---|
| basic | 称 32/32 | 96/102 |
| lua | 9/9 | 一致 |
| busybox | 55 条中 52 成功 | 51 成功 |
| libctest | 94 个用例，88 Pass | 107 个，98 Pass（用例集不同） |
| libcbench | 27 个 benchmark | 25 个 |
| lmbench | 约 17 项数字 | 11 项 |
| unixbench | 19 行结果 | 大量崩溃 |
| iozone | 子测试全部执行 | 大量崩溃 |
| cyclictest | 称 4/4 | 4 个 fail |
| ltp | 162 个用例，TPASS 54~60 | panic，没结果 |
| iperf / netperf | 脚本跑完，全部失败 | iperf 全失败，netperf 没跑到 |

后几行差距明显，可能是编译器不同（官方是 musl GCC 12.2，wonder-world 用 musl.cc 的 GCC 11.2.1）、盘布局不同，也可能是他的数字本身口径宽松。没有进一步确认。

## 已知问题

- **ltp 读大目录 panic**：`ltp/testcases/bin/` 有 2822 项（htree 目录，extent 有一层索引），`getdents64` 每次都把整个目录重新读一遍，读索引块（物理块 32599）时 virtio 报 `IoError`。原因未确认。
- **iozone、unixbench 崩溃在 `0xfffffffffffffffe`**：疑似子进程返回到 -2，和信号返回、fork 后的上下文有关，未查。
- **iperf 需要 `/dev/urandom`**：目前 `open` 返回 `Operation not permitted`。
- **usertests 会让内核 panic**（本地 easy-fs 镜像）：跑到 `race_adder_arg` 时 `src/mm/page_table.rs:115 vpn 0xffe is mapped before mapping`。`0xffe000` 是信号 stub 页，疑似多线程下重复映射，没有进一步确认。
- **EXT4「未写入的 extent」**（fallocate 预分配）被当作真实数据读，可能读到旧内容。目前没遇到，只是代码上看到。
- **每次 `read()` 都把整个文件重新读一遍**（`Ext4File::read` → `read_file`），大文件会很慢，也是上面大目录问题的放大因素。
- 官方脚本里 `busybox xxx`（不带 `./`）依赖 PATH，内核的环境变量是空的，目前靠 `execve` 找不到 `busybox` 时回退到 `musl/busybox` 绕过。

## 本分支为适配官方盘做的内核改动

1. EXT4 读稀疏文件的「洞」（读作全零）。
2. EXT4 按块组描述符读 inode，支持多块组大盘（原来只读块组 0）。
3. `chdir` 相对当前目录解析并处理 `.`、`..`。
4. shebang 回退到 busybox 时，在当前目录、根目录、`musl/` 下依次找。
5. 盘里没有 `oscomp_runner`/`autorun`/`initproc` 时使用编进内核的 runner，工作目录设为 `musl/`。
6. `execve` 找不到 `busybox` 时回退到 `musl/busybox`。
7. runner 的 lua 直驱特例只在跑 `lua_testcode.sh` 时触发，并输出 `lua-musl` 组名。

## wonder-world 文档里提到、仍未验证的限制

信号返回上下文被破坏会让 lmbench 的 `lat_ctx` 等子进程崩溃；单核调度下 cyclictest 压力阶段超时；没有动态链接；没有网卡和 TCP/IP 栈。
