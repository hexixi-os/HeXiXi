# Linux clone / pthread 支持现状

PR-13 在单核 QEMU 上验证了动态 musl、glibc 的 pthread 创建与回收、TLS、mutex、条件变量及定向信号。兼容范围包含同一地址空间的 futex WAIT、WAKE、BITSET 和 REQUEUE，以及 `exit_group` 清理。跨进程共享 futex、robust futex 的 owner-death 恢复、完整的信号栈与 siginfo ABI 仍待实现。运行结果见 [测试状态](TEST_STATUS.md)。

## 复跑 pthread 和动态 BusyBox

沿用 [PR-12 动态 ELF](TEST_STATUS.md#2026-11-24-pr-12-动态-elf) 的工具链和 fixture。其中 `busybox` 是原盘的静态程序，由 runner 用来启动测试。glibc 2.31 额外需要与 `libc.so.6` 同一套工具链的 `libpthread.so.0`。

```bash
source_file=tools/oscomp/tests/pthread_abi.c
"$musl_cc" -fPIE -pie -pthread -O2 "$source_file" -o "$fixture/pthread-musl"
"$glibc_cc" -fPIE -pie -pthread -O2 "$source_file" -o "$fixture/pthread-glibc"
for library in libpthread.so.0 libm.so.6 libresolv.so.2; do
    install -m755 "$("$glibc_cc" -print-file-name="$library")" "$fixture/lib/$library"
done
busybox_src=/path/to/writable/busybox
cp "$SUITE/config/busybox-config-riscv64" "$busybox_src/.config"
make -C "$busybox_src" CROSS_COMPILE=riscv64-linux-gnu- oldconfig
make -C "$busybox_src" CROSS_COMPILE=riscv64-linux-gnu- -j"$(nproc)"
install -m755 "$busybox_src/busybox" "$fixture/busybox-dynamic"
tools/oscomp/test_pthread_abi.sh "$fixture" "$fixture/pthread.log"
```

`busybox_src` 使用官方套件 BusyBox 源码的可写副本。`oldconfig` 可接受默认项，保持 `CONFIG_STATIC` 关闭。`readelf -l "$fixture/busybox-dynamic"` 应显示 `PT_INTERP`。

## 早期调试记录

> **历史记录**：这是 pthread 刚开始适配时的调试笔记，下面说的「父线程的 futex WAIT 没被唤醒」后来已经修通（见 [适配改动说明](ADAPTATION_NOTES.md)）。现状以代码和 [测试状态](TEST_STATUS.md) 为准。

## 已实现

- `clone(220)`：
  - 有 `CLONE_VM` 时在**当前进程内**建线程（共享 memory_set）；
  - 子线程 trap context = 父线程的副本，改 `x[2]=stack`、`x[4]=tls`、`x[10]=0`、
    `kernel_sp = 新 task 的 kstack top`、`sepc` 指向 ecall 之后；
  - 无 `CLONE_VM`（如 musl 的 `fork() = clone(SIGCHLD,0)`）时退回 `sys_fork()`。
- `futex(98)`：`FUTEX_WAIT` 用「让出 CPU + 复查内存值」实现；`FUTEX_WAKE` 直接返回。
- `TrapContext` 实现 `Copy`。

## 实测链路（最小 pthread 程序）

```c
pthread_create(&t, 0, f, (void*)42);
printf("create=%d\n", r);     // 打印 create=0，说明 clone 返回正确
pthread_join(t, &ret);          // ← 卡在这里
```

系统调用轨迹：

```
[trap] tid=0 UserEnvCall id=66   writev("create=0")
[trap] tid=0 UserEnvCall id=98   futex  ← 父线程进入 join 的等待
[trap] tid=1 UserEnvCall id=135  rt_sigprocmask
[trap] tid=1 UserEnvCall id=135  rt_sigprocmask
[trap] tid=1 UserEnvCall id=98   futex  ← 子线程退出前的唤醒
[trap] tid=1 UserEnvCall id=93   exit
```

结论：
1. 线程**能创建、能被调度、能执行用户态代码**（子线程跑了两条 sigprocmask 和 futex/exit）；
2. 子线程的 `printf` 输出没出现在屏幕上，可能是因为 musl 对非 tty 的 stdout 做了全缓冲，
   线程退出时不会 flush；
3. **父线程的 futex WAIT 没有被唤醒**：子线程已经写回并 wake，但父线程的轮询循环没有再观察到变化。

## 下一步排查建议

1. 在 `sys_futex` 的 WAIT 循环里打印 `uaddr / val / cur`，确认父线程读到的地址与子线程写入的
   地址是否同一个（排除「两份内存」问题）；
2. 检查 `suspend_current_and_run_next()` 之后父线程是否真的被重新调度（可能在 child exit 后
   调度器空转）；
3. 若确认数据一致但仍不唤醒，可改为「唤醒时把等待者显式标 Ready」的等待队列实现。
