TARGET := riscv64gc-unknown-none-elf
MODE := release
APP_DIR := src/bin
TARGET_DIR := target/$(TARGET)/$(MODE)
APPS := $(wildcard $(APP_DIR)/*.rs)

TEST ?=

build: $(APPS)
	@cargo build --release
ifeq ($(TEST), 1)
	@cp $(TARGET_DIR)/usertests $(TARGET_DIR)/initproc
endif

oscomp-runner:
	cargo build --release --target-dir target --bin oscomp_runner

clean:
	@cargo clean

.PHONY: build oscomp-runner clean
