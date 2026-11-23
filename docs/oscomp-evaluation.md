# 对接 oscomp 官方评测

## 评测机怎么跑

评测机在仓库根目录执行 `make all`，产出 `kernel-rv`，然后启动：

```bash
qemu-system-riscv64 -machine virt -kernel kernel-rv -m <mem> -nographic -smp <smp> \
  -bios default -drive file={fs},if=none,format=raw,id=x0 \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 -no-reboot \
  -device virtio-net-device,netdev=net -netdev user,id=net -rtc base=utc \
  -drive file=disk.img,if=none,format=raw,id=x1 \
  -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
```

`{fs}` 是官方提供的 **EXT4 镜像**（无分区表，4 GB），根目录下是编译好的程序和 `xxx_testcode.sh` 脚本。
内核需要在启动后：

1. 找到并读取这个磁盘；
2. 依次执行每个 `*_testcode.sh`；
3. 脚本自带 `#### OS COMP TEST GROUP START xxx ####` 之类的标记；
4. 全部跑完后主动关机。

**评分只看屏幕（串口）输出。**

## 内核现在怎么对接

| 步骤 | 实现 |
|---|---|
| 读 EXT4 | `os/src/fs/ext4.rs`，只读：超级块、块组、inode、extent、目录 |
| 选第一个进程 | `os/src/task/mod.rs` 的 `INITPROC`：盘里有 `oscomp_runner` 就启动它；都没有（官方盘就是这样）则用编进内核的 runner，工作目录设为 `musl/` |
| 执行脚本 | `user/src/bin/oscomp_runner.rs`：优先用盘里的 `busybox sh` 逐个执行 `*_testcode.sh` |
| 写文件 | EXT4 是只读的，写入走内存 overlay（`os/src/fs/mod.rs`），重启就没了 |
| 关机 | 第一个进程以 0 退出，内核调用 `exit_success` |

`oscomp_runner` 的两个特例：

- 盘里有 `lua` 和 `date.lua` 时，直接逐个 fork+exec `lua` 去跑每个 `.lua`，不走 `lua_testcode.sh`。
- 盘里没有 `busybox` 时，退回内置的迷你 shell，只支持少量语法。

**注意**：上面是 wonder-world 文档和本地模拟盘采用的**平铺**布局（`busybox`、`*_testcode.sh`、测试程序、`oscomp_runner` 都在根目录）。
真实官方盘的根目录是 `musl/`、`glibc/`、`lib/`、`lib64/`，盘里没有 `oscomp_runner`，脚本标记带 `-musl`/`-glibc` 后缀，当前的 `oscomp_runner` 对接方式在真实盘上不成立。详见 [开发与测试指南](DEVELOPMENT.md) 第 6 节。

## 在本地复现

构建、运行、判分的完整步骤见 [开发与测试指南](DEVELOPMENT.md)。当前各组的得分见 [测试状态](TEST_STATUS.md)。

## 已知限制

- EXT4 只读，写入不落盘。
- 单核，FIFO 轮转调度。
- 没有网卡驱动和 TCP/IP 协议栈，iperf、netperf 拿不到分。
- 不支持动态链接（没有 `ld-musl`，不支持 `dlopen`）。
- `execve` 不支持 `#!` 解释器行，所以 runner 是显式调用 `busybox sh <脚本>`，而不是直接执行脚本（wonder-world 记录，未单独验证）。
- 内存：lmbench 需要 1G，其余组 256M 够用。
