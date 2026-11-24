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
| 读 EXT4 | `os/src/fs/ext4.rs` 按块组查找 inode，稀疏洞和未写入 extent 读为零 |
| 选第一个进程 | `make all` 的 QEMU 评测构建先编译并内嵌 runner。`INITPROC` 识别官方布局后启动内嵌 runner |
| 执行脚本 | runner 进入 `/musl` 或 `/glibc`，用对应目录的 `busybox sh` 执行原盘脚本。串口保留脚本的组标记、用例输出和 runner 记录的脚本退出码 |
| 写文件 | EXT4 是只读的，写入走内存 overlay（`os/src/fs/mod.rs`），重启就没了 |
| 关机 | 第一个进程以 0 退出，内核调用 `exit_success` |

QEMU 支持 `-append "oscomp.group=basic-glibc"` 指定一组。省略参数时，runner 按内置顺序依次启动各组，卡住的脚本可能阻断后续组。宿主机矩阵工具为每组单独启动 QEMU，并单独计时。

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

## 完整评测矩阵

需要 Python 3.8+、Git 和 `qemu-system-riscv64`。用上节下载的原盘和测试套件，从仓库根目录运行。`--judges` 加载赛事公开协作仓库的四组判分器，basic 判分器来自测试套件。

```bash
git -C ~/testsuits-pre-20250615 checkout 4b40ee92e8b594fb20e3957ebc2f79ad43aca538
git clone https://github.com/oscomp/oskernel-testsuits-cooperation.git ~/oscomp-judges
git -C ~/oscomp-judges checkout 44fa7ac65f292580c3974a8252f533c4484ebe6b
python3 tools/oscomp/run_official_matrix.py ~/sdcard-rv.img ~/oscomp-work/official-matrix \
  --suite ~/testsuits-pre-20250615 --judges ~/oscomp-judges --timeout 120
```

在镜像路径和输出目录后加 `basic busybox-glibc` 可只运行 basic 的两个 libc 版本及 `busybox-glibc`。留空时运行 12 组的 musl、glibc 两个版本。每次启动使用原盘快照和新的 64 MiB 临时盘，默认单核、1 GiB、每组 200 秒。`--kernel` 可复用内核，省略时由 `make all` 构建。官方盘的 lmbench 保留原始计时参数，本地模拟盘构建脚本单独设置 `ENOUGH=10000`。

每次运行生成一个 `run.*` 目录。`run.meta.json` 记录源码和判分器的版本、原盘与内核的 SHA-256、QEMU 版本。`matrix.json` 在每组结束时更新。每组目录保存以下文件。

| 文件 | 内容 |
|---|---|
| `serial.log` | 完整的 QEMU 串口及诊断输出 |
| `group.log` | 该组起止标记之间的输出，超时保留已输出部分 |
| `result.json` | QEMU 命令、运行时间、脚本退出码、日志覆盖状态、超时、panic 和可提取的用例结果 |
| `score.json` | 可用判分器返回的逐项成绩 |

组状态 `complete` 表示标记完整、脚本退出码为 0 且正常关机，仍需查看判分或用例结果。`script_error` 表示脚本非零退出。`timeout`、`panic`、`qemu_error`、`launcher_error` 和 `incomplete` 分别记录超时、内核 panic、QEMU 异常、启动器失败和记录不完整。判分器可对超时前的输出评分，缺少判分器时记录 `unavailable`。矩阵命令的退出码用于报告工具或判分器错误，各组运行结果见 `matrix.json`。本地初始结果见 [测试状态](TEST_STATUS.md#2026-11-24-pr-11-官方原盘评测基线)。

## 已知限制

- EXT4 只读，写入由内存 overlay 提供。
- 当前为 RISC-V64 单核构建，使用 FIFO 轮转调度。
- 网卡驱动、TCP/IP 协议栈和动态 ELF 解释器加载仍待实现。
- 本地矩阵接入 basic、busybox、lua、libctest、iozone 五组的公开判分器。其余组保留原始输出和可提取的用例结果，尚无本地总分。
