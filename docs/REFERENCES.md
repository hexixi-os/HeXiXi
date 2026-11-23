# 参考链接

项目用到的工具、测试套件和资料的官方地址，方便查阅。链接最后核对于 2026-11-23，均可访问。

## 比赛与评测

| 名称 | 地址 | 说明 |
|---|---|---|
| 大赛平台 | http://course.educg.net | 提交评测的地方 |
| 官方测试套件（GitHub） | https://github.com/oscomp/testsuits-for-oskernel | 本项目用 `2025_multiarch` 分支，其他分支是别的赛题 |
| 官方测试套件（Gitee 镜像） | https://gitee.com/oscomp/testsuits-for-oskernel | GitHub 克隆失败时用这个 |
| 测例说明（志愿者维护） | https://github.com/oscomp/oskernel-testsuits-cooperation | 每组测例的题目描述、编译方法、样例输出、评分依据 |
| 评测用 Docker 镜像 | https://hub.docker.com/r/alicesama/os-contest-image | 本项目用 `v0.2`，内含 musl/glibc 交叉编译器和 QEMU 9.2.1 |
| 评测镜像 Dockerfile | https://gitlab.educg.net/wangmingjian/os-contest-2024-image/ | 官方 README 给出的地址 |
| oscomp 组织 | https://github.com/oscomp | 往年赛题和资料 |

## 内核基础（上游与教程）

| 名称 | 地址 | 说明 |
|---|---|---|
| rCore-Tutorial-v3 | https://github.com/rcore-os/rCore-Tutorial-v3 | 本项目的上游，当前基于它的 ch8 |
| rCore-Tutorial 教程 | https://rcore-os.cn/rCore-Tutorial-Book-v3/ | 配套教程，读懂内核结构从这里开始 |
| rcore-os/riscv | https://github.com/rcore-os/riscv | 内核依赖的 RISC-V 寄存器封装库 |
| RISC-V 指令集手册 | https://github.com/riscv/riscv-isa-manual | 特权架构、页表（Sv39）、陷阱的权威说明 |
| RISC-V SBI 规范 | https://github.com/riscv-non-isa/riscv-sbi-doc | 内核与固件之间的接口 |
| OpenSBI | https://github.com/riscv-software-src/opensbi | QEMU `-bios default` 用的固件 |
| Linux 系统调用列表 | https://man7.org/linux/man-pages/man2/syscalls.2.html | 查系统调用语义，看手册页即可 |
| RISC-V 系统调用号表 | https://filippo.io/linux-syscall-table/ | 按架构查调用号（选 riscv64 对照） |

## 开发工具

| 名称 | 地址 | 说明 |
|---|---|---|
| rustup | https://rustup.rs | Rust 工具链管理器，项目锁定 `nightly-2022-08-05` |
| cargo-binutils | https://github.com/rust-embedded/cargo-binutils | 提供 `rust-objcopy`，本项目用 0.3.3 |
| QEMU（RISC-V 说明） | https://www.qemu.org/docs/master/system/target-riscv.html | `virt` 机器和启动参数 |
| GDB | https://sourceware.org/gdb/ | 我们用 `gdb-multiarch` 调试内核 |
| musl 交叉编译器 | https://musl.cc | 本地脚本用 `riscv64-linux-musl-cross`；官方镜像里是另一个版本 |
| CMake 发布页 | https://github.com/Kitware/CMake/releases | basic 组用 CMake 构建，`setup.sh` 下载这里的预编译包 |
| USTC crates 镜像说明 | https://mirrors.ustc.edu.cn/help/crates.io-index.html | 国内 cargo 源，配置见 [BUILD.md](BUILD.md) |

## 各评测组的上游项目

测试套件里的程序都来自下面这些项目，遇到行为不符合预期时，到上游看它真正期望什么。

| 评测组 | 项目地址 |
|---|---|
| basic | 官方自带，源码在套件的 `basic/user/src/oscomp/`，说明见上面的「测例说明」仓库 |
| busybox | https://busybox.net |
| lua | https://www.lua.org |
| libc-test | https://wiki.musl-libc.org/libc-test （musl 官方的 libc 测试集） |
| libc-bench | 套件内自带，源码在 `libc-bench/` |
| iozone | https://www.iozone.org |
| unixbench | https://github.com/kdlucas/byte-unixbench |
| lmbench | https://github.com/intel/lmbench 、https://lmbench.sourceforge.net |
| iperf | https://github.com/esnet/iperf （iperf3） |
| netperf | https://github.com/HewlettPackard/netperf |
| cyclictest | https://git.kernel.org/pub/scm/utils/rt-tests/rt-tests.git （rt-tests，含 cyclictest 和 hackbench） |
| ltp | https://github.com/linux-test-project/ltp |

## 本项目相关

| 名称 | 地址 |
|---|---|
| 本仓库 | https://github.com/hexixi-os/HeXiXi |
