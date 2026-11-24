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

`{fs}` 是官方提供的 **EXT4 镜像**。2025 初赛 RISC-V64 发布盘的程序和测试脚本位于 `/musl`、`/glibc`。脚本在串口输出组标记和用例结果，判分器据此执行断言。

## 内核现在怎么对接

| 步骤 | 实现 |
|---|---|
| 读 EXT4 | `os/src/fs/ext4.rs` 按块组查找 inode，稀疏洞和未写入 extent 读为零；文件页走页缓存 |
| 选第一个进程 | `make all` 的 QEMU 评测构建先编译并内嵌 runner。`INITPROC`：盘里有 `oscomp_runner` 就启动它；都没有（官方盘就是这样）则用编进内核的 runner，并把工作目录设为 `musl/` |
| 执行脚本 | `user/src/bin/oscomp_runner.rs` 优先用盘里的 `busybox sh` 逐个执行 `*_testcode.sh`，保留原版组标记和用例输出 |
| 写文件 | EXT4 是只读的，写入走内存 overlay（`os/src/fs/mod.rs`），重启就没了 |
| 关机 | 第一个进程以 0 退出，内核调用 `exit_success` |

平铺模拟盘继续从根目录加载 `oscomp_runner`。教学盘可通过 `run_tests` 启动 `autorun`，默认启动 `initproc`。

## 官方盘回归

在 Linux 或 WSL2 配好 [构建环境](BUILD.md)，并从 [2025 初赛镜像发布页](https://github.com/oscomp/testsuits-for-oskernel/releases/tag/pre-20250615) 下载 `sdcard-rv.img.xz` 到 `~`。以下命令从 HeXiXi 仓库根目录运行。

```bash
xz -dk ~/sdcard-rv.img.xz
git clone --depth 1 --branch pre-20250615 https://github.com/oscomp/testsuits-for-oskernel.git ~/testsuits-pre-20250615
SUITE=~/testsuits-pre-20250615 tools/oscomp/run_official_basic.sh ~/sdcard-rv.img ~/oscomp-work/official-basic
```

脚本用 `make all` 构建内核，以 QEMU 快照模式挂载原盘。每次运行在结果目录生成独立记录，包含 `run.meta`、`serial.log`、`basic-musl.log` 和 `score.txt`。`run.meta` 记录镜像与内核 SHA-256、当前源码与测试套件提交、QEMU 命令、运行耗时和退出状态。完成回归要求串口有完整的组标记和用例输出，且 QEMU 正常退出。部分断言失败会保留分数和用例详情。

已有内核可通过 `KERNEL=/path/to/kernel-rv` 复用。`QEMU_MEM` 默认 `1G`，`QEMU_TIMEOUT` 默认 `200` 秒。仅验证 EXT4 读取时，有 `debugfs` 和 stable Rust 的宿主环境可执行 `tools/oscomp/test_ext4.sh ~/sdcard-rv.img`。实测数据见 [测试状态](TEST_STATUS.md)。

## 已知限制

- EXT4 只读，写入不落盘。
- 单核，FIFO 轮转调度。
- 没有网卡驱动和 TCP/IP 协议栈，iperf、netperf 拿不到分。
- 不支持动态链接（没有 `ld-musl`，不支持 `dlopen`）。
- 官方盘回归当前覆盖 `basic-musl`。动态 glibc 和其他组仍待适配或复跑。
