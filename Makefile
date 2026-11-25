# rCore-Tutorial-v3 / RUST_CORE
#
# 官方评测契约：评测机在仓库根目录执行 `make all`，
# 并取用生成的 kernel-rv 作为内核镜像。
# 本地开发仍建议进 os/ 目录用 `make run`。

.PHONY: all kernel-rv kernel-la clean-user fmt

KERNEL_BIN := os/target/riscv64gc-unknown-none-elf/release/os.bin
KERNEL_OUT ?= kernel-rv

all: kernel-rv

kernel-rv:
	$(MAKE) -C os env
	$(MAKE) -C os kernel-bin OSCOMP=1
	cp $(KERNEL_BIN) "$(KERNEL_OUT)"
	@echo "$(KERNEL_OUT): $$(wc -c < "$(KERNEL_OUT)") bytes"

LA_TARGET := loongarch64-unknown-none-softfloat
LA_TOOLCHAIN ?= 1.98.1
LA_KERNEL_OUT ?= kernel-la

kernel-la:
	cd user && RUSTC_BOOTSTRAP=user_lib cargo +$(LA_TOOLCHAIN) build --locked --release --target $(LA_TARGET) --bin oscomp_runner
	cd os && RUSTC_BOOTSTRAP=os cargo +$(LA_TOOLCHAIN) build --locked --release --target $(LA_TARGET) --features oscomp_runner
	cp os/target/$(LA_TARGET)/release/os "$(LA_KERNEL_OUT)"

clean-user:
	$(MAKE) -C os clean
	$(MAKE) -C user clean
	rm -f kernel-rv kernel-la

fmt:
	cd easy-fs; cargo fmt; cd ../easy-fs-fuse; cargo fmt; cd ../os; cargo fmt; cd ../user; cargo fmt; cd ..
