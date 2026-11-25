# Building (QEMU RISC-V64)
TARGET := riscv64gc-unknown-none-elf
MODE := release
KERNEL_ELF := target/$(TARGET)/$(MODE)/os
KERNEL_BIN := $(KERNEL_ELF).bin
DISASM_TMP := target/$(TARGET)/$(MODE)/asm
FS_IMG := ../user/target/$(TARGET)/$(MODE)/fs.img
APPS := ../user/src/bin/*

OBJDUMP := rust-objdump --arch-name=riscv64
OBJCOPY := rust-objcopy --binary-architecture=riscv64
DISASM ?= -x
TEST ?=

FEATURES :=
ifeq ($(SMP),1)
FEATURES += smp
endif
QEMU_SMP ?= $(if $(filter 1,$(SMP)),2,1)
PAGE_CACHE_POLICY ?= lru
ifeq ($(PAGE_CACHE_POLICY),none)
FEATURES += page_cache_none
else ifeq ($(PAGE_CACHE_POLICY),adaptive)
FEATURES += page_cache_adaptive
else ifneq ($(PAGE_CACHE_POLICY),lru)
$(error PAGE_CACHE_POLICY must be lru, adaptive or none)
endif
ifeq ($(ELF_STARTUP_STATS),1)
FEATURES += elf_startup_stats
endif
ifeq ($(SCHED_SWITCH_STATS),1)
FEATURES += sched_switch_stats
endif
ifeq ($(OSCOMP),1)
FEATURES += oscomp_runner
kernel: oscomp-runner
oscomp-runner:
	$(MAKE) -C ../user oscomp-runner
endif

build: env $(KERNEL_BIN) fs-img

kernel-bin: $(KERNEL_BIN)

env:
	(rustup target list | grep "riscv64gc-unknown-none-elf (installed)") || rustup target add $(TARGET)
	command -v rust-objcopy >/dev/null 2>&1 || cargo install cargo-binutils --version 0.3.3
	rustup component add rust-src
	rustup component add llvm-tools-preview

$(KERNEL_BIN): kernel
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $@

fs-img: $(APPS)
	@cd ../user && make build TEST=$(TEST)
	@rm -f $(FS_IMG)
	@cd ../easy-fs-fuse && cargo +stable run --release -- -s ../user/src/bin/ -t ../user/target/riscv64gc-unknown-none-elf/release/

$(APPS):

kernel:
	@cp src/linker-qemu.ld src/linker.ld
	@cargo build --release --features "$(FEATURES)"
	@rm src/linker.ld

clean:
	@cargo clean

disasm: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) | less

disasm-vim: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) > $(DISASM_TMP)
	@vim $(DISASM_TMP)
	@rm $(DISASM_TMP)

run: build
	@qemu-system-riscv64 \
		-machine virt \
		-smp $(QEMU_SMP) \
		-m 256M \
		-nographic \
		-bios default \
		-kernel $(KERNEL_BIN) \
		-drive file=$(FS_IMG),if=none,format=raw,id=x0 \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0

debug: build
	@tmux new-session -d \
		"qemu-system-riscv64 -machine virt -nographic -bios default -kernel $(KERNEL_BIN) -s -S" && \
		tmux split-window -h "riscv64-unknown-elf-gdb -ex 'file $(KERNEL_ELF)' -ex 'set arch riscv:rv64' -ex 'target remote localhost:1234'" && \
		tmux -2 attach-session -d

gdbserver: build
	@qemu-system-riscv64 -machine virt -nographic -bios default -kernel $(KERNEL_BIN) -s -S

gdbclient:
	@riscv64-unknown-elf-gdb -ex 'file $(KERNEL_ELF)' -ex 'set arch riscv:rv64' -ex 'target remote localhost:1234'

.PHONY: build env kernel-bin kernel oscomp-runner clean disasm disasm-vim run debug fs-img gdbserver gdbclient
