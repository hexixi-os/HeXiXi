# rCore-Tutorial-v3 / RUST_CORE
#
# 官方评测契约：评测机在仓库根目录执行 `make all`，
# 并取用生成的 kernel-rv 作为内核镜像。
# 本地开发仍建议进 os/ 目录用 `make run`。

.PHONY: all kernel-rv clean-user docker build_docker fmt

DOCKER_NAME ?= rcore-tutorial-v3
KERNEL_BIN := os/target/riscv64gc-unknown-none-elf/release/os.bin
KERNEL_OUT ?= kernel-rv

all: kernel-rv

kernel-rv:
	$(MAKE) -C os env
	$(MAKE) -C os kernel-bin BOARD=qemu OSCOMP=1
	cp $(KERNEL_BIN) "$(KERNEL_OUT)"
	@echo "$(KERNEL_OUT): $$(wc -c < "$(KERNEL_OUT)") bytes"

clean-user:
	$(MAKE) -C os clean
	$(MAKE) -C user clean
	rm -f kernel-rv

docker:
	docker run --rm -it -v ${PWD}:/mnt -w /mnt ${DOCKER_NAME} bash

build_docker:
	docker build -t ${DOCKER_NAME} .

fmt:
	cd easy-fs; cargo fmt; cd ../easy-fs-fuse; cargo fmt; cd ../os; cargo fmt; cd ../user; cargo fmt; cd ..
