# 核夕夕 HeXiXi

基于 [rCore-Tutorial-v3](https://github.com/rcore-os/rCore-Tutorial-v3) 的 Rust 操作系统内核，支持在 QEMU 的 RISC-V64（单核 / 双核）与 LoongArch64（单核）平台上运行全国大学生操作系统比赛（oscomp）官方测例。

## 当前能力

- **架构与多核**：RISC-V64 默认单核，支持 `SMP=1` 双核调度与远程 TLB 同步；LoongArch64 支持单核 ELF 直启与 VirtIO PCI。
- **内存与装载**：按需 ELF 加载、`fork` 写时复制（CoW）、文件/匿名 `mmap` 与 VMA 拆分、共享页缓存（`lru` / `adaptive` / `none`）、动态 ELF 解释器（musl / glibc）。
- **进程与线程**：实现 100+ Linux 系统调用，支持 `clone` / 进程内与跨进程共享 `futex` / TLS / 信号投递 / `pthread` 与线程组退出。
- **文件系统与运行环境**：VFS 挂载表（`mount` / `umount2` / `statfs`）、只读 EXT4 + 可写内存 Overlay、独立 `tmpfs`（`/tmp`、`/var/tmp`、`/dev/shm`）、符号链接、`/proc/self` 与 `/dev/{null,zero,random,urandom,tty}` 及 `/lib`↔`/lib64` 兼容链接。
- **评测对接**：`make all` 产出内嵌 `oscomp_runner` 的 `kernel-rv`。2025 初赛原盘可通过 `--legacy-image` 运行 `/musl` 与 `/glibc` 脚本，见[评测文档](docs/oscomp-evaluation.md)。网卡驱动与 TCP/IP 协议栈待实现。

> 分支说明：`oscomp-adaptation` 为主开发分支，`main` 为上游原版教学内核基线。

## 快速开始

在 Linux / Ubuntu（或 WSL2）下执行，环境配置见 [构建与运行](docs/BUILD.md)。

```bash
# 构建 RISC-V64 评测内核（单核 / 双核）
make all
make all SMP=1

# 构建 LoongArch64 评测内核
make kernel-la

# 官方盘 basic 回归
SUITE=~/testsuits-pre-20250615 tools/oscomp/run_official_basic.sh ~/sdcard-rv.img ~/oscomp-work/official-basic
```

## 目录结构

```
os/             内核源码（arch/、boards/、drivers/、fs/、mm/、sync/、syscall/、task/、trap/）
user/           用户态库、回归测试程序与 oscomp_runner
easy-fs/        easy-fs 文件系统库（教学盘 / usertests）
easy-fs-fuse/   easy-fs 镜像打包工具
tools/oscomp/   官方盘评测矩阵、本地模拟盘构建与专项回归脚本
docs/           项目文档
```

## 文档

| 文档 | 内容 |
|---|---|
| [构建与运行](docs/BUILD.md) | 环境安装、RISC-V64 / LoongArch64 / SMP 构建、usertests 与常见问题 |
| [开发与测试指南](docs/DEVELOPMENT.md) | 内核模块、启动流程、调试方法、本地模拟盘与专项回归 |
| [对接官方评测](docs/oscomp-evaluation.md) | 评测机契约、官方原盘回归与 24 项评测矩阵 |
| [测试状态](docs/TEST_STATUS.md) | 当前各平台与各专项回归的实测结果与已知限制 |
| [参考链接](docs/REFERENCES.md) | 赛事仓库、内核资料、工具链与测例上游地址 |

## 许可证

沿用上游许可证，见 [LICENSE](LICENSE)。
