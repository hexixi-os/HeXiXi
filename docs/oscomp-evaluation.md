# 对接 oscomp 官方评测

## 1. 评测机契约

评测机在仓库根目录执行 `make all` 产出 `kernel-rv`，并启动 QEMU 挂载官方 EXT4 镜像（`/musl`、`/glibc` 目录），通过串口输出的 `#### OS COMP TEST GROUP START/END ####` 与用例日志判分：

```bash
qemu-system-riscv64 -machine virt -kernel kernel-rv -m <mem> -nographic -smp <smp> \
  -bios default -drive file={fs},if=none,format=raw,id=x0 \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 -no-reboot \
  -device virtio-net-device,netdev=net -netdev user,id=net -rtc base=utc \
  -drive file=disk.img,if=none,format=raw,id=x1 \
  -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
```

内核对接要点：
- **只读 EXT4 + 内存 Overlay**：`os/src/fs/ext4.rs` 负责块组/稀疏文件读取与统一页缓存，`os/src/fs/overlay.rs` 承接全部写入（关机不落盘）。
- **内嵌 `oscomp_runner`**：`make all` 与 `make kernel-la` 开启 `oscomp_runner` feature，识别官方盘布局后进入 `/musl`、`/glibc` 依次用 BusyBox 执行脚本；支持 `-append "oscomp.group=<组名>"` 指定单组运行。
- **正常关机**：首个进程退出码为 0 时调用平台关机接口。

## 2. 官方原盘回归（basic）

从 [2025 初赛镜像发布页](https://github.com/oscomp/testsuits-for-oskernel/releases/tag/pre-20250615) 下载 `sdcard-rv.img.xz`（或 LoongArch64 版 `sdcard-la.img.xz`）：

```bash
xz -dk ~/sdcard-rv.img.xz
git clone --depth 1 --branch pre-20250615 https://github.com/oscomp/testsuits-for-oskernel.git ~/testsuits-pre-20250615

# RISC-V64 basic-musl 回归（支持 KERNEL=、QEMU_MEM=、QEMU_TIMEOUT=）
SUITE=~/testsuits-pre-20250615 tools/oscomp/run_official_basic.sh ~/sdcard-rv.img ~/oscomp-work/official-basic
```

每次运行生成独立目录，保存 `run.meta`（镜像/内核 SHA-256、commit、耗时）、`serial.log`、`basic-musl.log` 与 `score.txt`。

## 3. 完整 24 项评测矩阵

`tools/oscomp/run_official_matrix.py` 为每个测试组独立启动 QEMU 并记录 `run.meta.json`、`matrix.json` 及每组的 `serial.log` / `group.log` / `result.json` / `score.json`：

```bash
git -C ~/testsuits-pre-20250615 checkout 4b40ee92e8b594fb20e3957ebc2f79ad43aca538
git clone https://github.com/oscomp/oskernel-testsuits-cooperation.git ~/oscomp-judges
git -C ~/oscomp-judges checkout 44fa7ac65f292580c3974a8252f533c4484ebe6b

# 运行全部 24 项（12 组 × musl/glibc），也可在末尾指定组名如 basic busybox-glibc
python3 tools/oscomp/run_official_matrix.py ~/sdcard-rv.img ~/oscomp-work/official-matrix \
  --suite ~/testsuits-pre-20250615 --judges ~/oscomp-judges --timeout 120

# RISC-V64 双核 / LoongArch64 单核 basic
python3 tools/oscomp/run_official_matrix.py --smp 2 ~/sdcard-rv.img /tmp/rv-smp basic \
  --suite ~/testsuits-pre-20250615 --kernel kernel-rv
python3 tools/oscomp/run_official_matrix.py --arch loongarch64 ~/sdcard-la.img /tmp/la-basic basic \
  --suite ~/testsuits-pre-20250615 --kernel kernel-la
```

本地矩阵已接入 `basic`、`busybox`、`lua`、`libctest`、`iozone` 五组公开判分器，其余组保留原始串口日志与可提取结果。

## 4. 已知限制

- EXT4 为只读，写入仅保存在内存 Overlay（数据页预算为 RAM/4，上限 128 MiB）。
- 网卡驱动与 TCP/IP 协议栈尚未实现（`iperf` / `netperf` 无法得分）。
- RISC-V64 SMP 仍使用全局内核锁串行化内核态，同一进程的线程绑定在同一 hart；LoongArch64 仅支持单核。
- 跨进程共享 futex、robust futex owner-death 恢复及完整 siginfo 队列待实现。
