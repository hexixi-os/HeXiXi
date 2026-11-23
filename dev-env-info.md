# 开发环境版本

实测可用的组合（2026-11-22，WSL2）。安装步骤见 [docs/BUILD.md](docs/BUILD.md)，用法见 [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)。

| 组件 | 版本 |
|---|---|
| 系统 | Ubuntu 26.04（WSL2） |
| Rust（内核、用户程序） | `nightly-2022-08-05`（rustc 1.64.0-nightly），由 `rust-toolchain.toml` 锁定 |
| Rust（宿主工具） | stable |
| cargo-binutils | 0.3.3 |
| QEMU | 10.2.1 |
| OpenSBI | 1.8（QEMU 自带，`-bios default`） |
| musl 交叉编译器 | GCC 11.2.1（musl.cc） |
| cmake | 3.31.6 |
| 官方测试套件 | `testsuits-for-oskernel` 分支 `2025_multiarch`，提交 `9ac474b` |
