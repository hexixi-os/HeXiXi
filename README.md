# 核夕夕 HeXiXi

基于 [rCore-Tutorial-v3](https://github.com/rcore-os/rCore-Tutorial-v3) 的 RISC-V64 教学内核，
已改造成能跑「全国大学生操作系统比赛（oscomp）」官方测例。运行在 QEMU 里。

## 现在是什么状态

- 内核约 7700 行 Rust，实现了 100 多个 Linux 系统调用（`fork`/`exec`/`clone`/`futex`/`mmap`/信号等）。
- 文件系统：只读 EXT4（评测盘）。写入的文件只存在内存里，重启就没了。
- 单核，没有网卡驱动和网络协议栈，不支持动态链接。
- 评测方式：看串口输出。内核启动后自动执行测试盘里的 `*_testcode.sh`，跑完关机。
- **基线**（`feat/official-disk` 分支，真实官方盘 musl 部分）：basic 96/102、lua 9/9、busybox 51/55、libctest 98/107。unixbench、iozone 大量崩溃，ltp 会让内核 panic，iperf 全失败。详见 [测试状态](docs/TEST_STATUS.md)。

当前分支 `oscomp-adaptation` 是适配分支；`main` 仍是原版基线，没有适配评测。

## 快速开始

在 Ubuntu / WSL2 里（Windows 原生不支持）。环境怎么装、每个脚本干什么，见 [开发与测试指南](docs/DEVELOPMENT.md)。

```bash
tools/oscomp/setup.sh                 # 下载测试套件、交叉编译器、cmake
tools/oscomp/build_runner.sh          # 用户态启动程序 oscomp_runner
tools/oscomp/build_busybox.sh         # busybox
tools/oscomp/build_tests.sh           # basic 组的 32 个用例
tools/oscomp/make_local_image.sh      # 打成 EXT4 测试盘
make -C os build                      # 编译内核
tools/oscomp/run_group.sh ~/oscomp-work/local/disk.img 256M 200 ~/oscomp-work/local/basic.log
tools/oscomp/score_basic.sh ~/oscomp-work/local/basic.log     # 官方判分器打分
```

## 目录结构

```
os/             内核：内存 mm/、陷阱 trap/、进程线程 task/、系统调用 syscall/、文件系统 fs/、驱动 drivers/
user/           用户态库和程序；src/bin/oscomp_runner.rs 是内核启动的第一个进程
easy-fs/        原版 rCore 的文件系统库（评测路线基本用不到）
easy-fs-fuse/   把用户程序打成 easy-fs 镜像的宿主工具
bootloader/     预编译 RustSBI 固件（已不用，改用 QEMU 自带 OpenSBI）
tools/oscomp/   测试盘构建、运行、判分脚本
docs/           文档
```

## 文档

| 文档 | 内容 |
|---|---|
| [开发与测试指南](docs/DEVELOPMENT.md) | **先看这个**：环境、模块、开发流程、调试、测试原理 |
| [测试状态](docs/TEST_STATUS.md) | 各评测组的实测分数和已知问题 |
| [构建与运行](docs/BUILD.md) | 环境安装；在本地镜像上跑 usertests |
| [对接官方评测](docs/oscomp-evaluation.md) | 评测机怎么跑、内核怎么对接 |
| [参考链接](docs/REFERENCES.md) | 各工具、测试套件、上游项目和规范的官方地址 |
| [适配改动说明](docs/ADAPTATION_NOTES.md) | 历史记录：每一轮适配改了什么（数字未经 Martyr 复现） |
| [部署笔记](docs/deploy-notes.md)、[线程笔记](docs/thread-support.md) | 历史记录，仅供参考 |

## 许可证

沿用上游，见 [LICENSE](LICENSE)。
