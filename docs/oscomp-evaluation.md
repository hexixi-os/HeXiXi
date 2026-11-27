# 对接 oscomp 官方评测

## 1. 评测机契约

评测机在仓库根目录执行 `make all` 产出 `kernel-rv`，启动 QEMU 挂载官方 EXT4 镜像（`/musl`、`/glibc`），通过串口输出的 `#### OS COMP TEST GROUP START/END ####` 与用例日志判分：

```bash
qemu-system-riscv64 -machine virt -kernel kernel-rv -m <mem> -nographic -smp <smp> \
  -bios default -drive file={fs},if=none,format=raw,id=x0 \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 -no-reboot \
  -device virtio-net-device,netdev=net -netdev user,id=net -rtc base=utc \
  -drive file=disk.img,if=none,format=raw,id=x1 \
  -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
```

内核对接要点：
- **只读 EXT4 + 内存 Overlay**：`os/src/fs/ext4.rs` 负责只读 EXT4 与统一页缓存，`os/src/fs/overlay.rs` 承接根盘写入；可选第二块 VirtIO 数据盘挂载 EasyFS。
- **内嵌 `oscomp_runner`**：`make all` / `make kernel-la` 开启 `oscomp_runner` feature，识别官方盘后依次执行 `/musl`、`/glibc` 脚本；支持 `-append "oscomp.group=<组名>"` 指定单组。
- **正常关机**：首个进程退出码为 0 时调用平台关机接口。

## 2. 官方原盘回归（basic）

从 [2025 初赛镜像发布页](https://github.com/oscomp/testsuits-for-oskernel/releases/tag/pre-20250615) 下载 `sdcard-rv.img.xz`（或 `sdcard-la.img.xz`）：

```bash
xz -dk ~/sdcard-rv.img.xz
git clone --depth 1 --branch pre-20250615 https://github.com/oscomp/testsuits-for-oskernel.git ~/testsuits-pre-20250615

# RISC-V64 basic-musl 回归（支持 KERNEL=、QEMU_MEM=、QEMU_TIMEOUT=）
SUITE=~/testsuits-pre-20250615 tools/oscomp/run_official_basic.sh ~/sdcard-rv.img ~/oscomp-work/official-basic
```

每次运行生成独立目录，保存 `run.meta`、`serial.log`、`basic-musl.log` 与 `score.txt`。

## 3. 完整 24 项评测矩阵

`tools/oscomp/run_official_matrix.py` 为每个测试组独立启动 QEMU，记录 `run.meta.json`、`matrix.json` 及每组的 `serial.log` / `group.log` / `result.json` / `score.json`（已接入 `basic`、`busybox`、`lua`、`libctest`、`iozone` 公开判分器）。

- `--legacy-image`：复跑 2025 初赛原盘时启用（设置 `oscomp.legacy=1` / `opt/hexixi/oscomp.legacy=1`），开启旧镜像的 `mnt` 处理、脚本回退与 basic PIE 直启；内核默认使用标准路径语义。
- `--strict`：要求所有选中组正常完成退出，且选中的 `basic` 组均达到 `102/102`。
- `unixbench` 原脚本串行执行 27 项，采样参数合计 390 s。用 `--timeout 120` 比较矩阵覆盖范围时会截断。完整运行可为该组单独设置更长的宿主机超时。原盘还缺少三个 SHELL 用例依赖的 `sort.src`，可将测试套件的 `scripts/unixbench/sort.src` 放入诊断镜像副本的 `/musl` 和 `/glibc`，保留原脚本的采样参数。

```bash
git -C ~/testsuits-pre-20250615 checkout 4b40ee92e8b594fb20e3957ebc2f79ad43aca538
git clone https://github.com/oscomp/oskernel-testsuits-cooperation.git ~/oscomp-judges
git -C ~/oscomp-judges checkout 44fa7ac65f292580c3974a8252f533c4484ebe6b

# 全部 24 项（也可在末尾指定组名如 basic busybox-glibc）
python3 tools/oscomp/run_official_matrix.py ~/sdcard-rv.img ~/oscomp-work/official-matrix \
  --suite ~/testsuits-pre-20250615 --judges ~/oscomp-judges --legacy-image --timeout 120

# RISC-V64 双核采样与 LoongArch64 单核门禁
python3 tools/oscomp/run_official_matrix.py --smp 2 ~/sdcard-rv.img /tmp/rv-smp basic \
  --suite ~/testsuits-pre-20250615 --kernel kernel-rv --legacy-image
python3 tools/oscomp/run_official_matrix.py --arch loongarch64 ~/sdcard-la.img /tmp/la-basic basic \
  --suite ~/testsuits-pre-20250615 --kernel kernel-la --legacy-image --strict
```

## 4. 网络与持久化回归

```bash
# IPv4 回环与 VirtIO TCP 回显（客机地址 10.0.2.15/24）
bash tools/oscomp/test_net_abi.sh all
bash tools/oscomp/test_net_abi.sh --virtio --smp 2 riscv64
bash tools/oscomp/test_net_abi.sh --virtio loongarch64

# EasyFS 持久化数据盘（4 次重启校验，手动挂载见 docs/BUILD.md#4-easyfs-持久化数据盘）
bash tools/oscomp/test_persistent_fs.sh all /tmp/hexixi-persistent-fs
QEMU_SMP=2 bash tools/oscomp/test_persistent_fs.sh riscv64 /tmp/hexixi-persistent-fs-smp
```

> 原盘 `iperf` 使用回环地址；`netperf` 需将对应架构静态编译的 `netperf`、`netserver`、BusyBox 与 `netperf_testcode.sh` 打入独立 EXT4 盘并以 `netperf` 组运行。

## 5. 已知限制

- EXT4 为只读，根盘写入保存在内存 Overlay（与 `tmpfs` 共享 RAM/4 预算，上限 128 MiB，关机丢失）；持久化数据盘采用 EasyFS（无日志与自动回写），暂不支持可写 EXT4。
- 网络支持 IPv4 和本地 Unix 套接字。VirtIO 网络仍通过轮询驱动。
- RISC-V64 SMP 使用全局内核锁串行化内核态，同一进程线程绑定在同一 hart；LoongArch64 仅支持单核。
- 信号投递支持基础 `siginfo`，实时信号的完整排队语义仍待实现。
- `mremap` 以现有 SysV 共享内存附件为源映射时返回 `EINVAL`。
