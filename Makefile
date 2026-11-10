# VelaOS developer entry points.
#
# The real build logic lives in `tools/xtask`, because a build step that
# compiles the kernel is a program, not a shell one-liner. This Makefile is
# only a discoverable front door: `make run` boots the OS, and the other
# targets name the checks CI runs. Every target forwards to xtask or cargo, so
# there is exactly one implementation of each step.

CARGO ?= cargo
XTASK := $(CARGO) xtask
ARCH ?= x86-64
PROFILE ?= release
TIMEOUT ?= 60

# The four architectures CI compile-checks. x86-64 has the working UEFI
# target and the drivers; the other three carry real architecture layers
# (paging, context switch, syscall gate, interrupt controller, timer and
# console) that are compiled for their own targets, so a change that breaks
# a port is caught here rather than on the machine that has to boot it.

.PHONY: help run build image check test test-boot test-gui fmt clippy clean

help:
	@echo "VelaOS"
	@echo ""
	@echo "  make run         build an image and boot it under QEMU with a GUI"
	@echo "  make build       compile the loader, kernel and userland"
	@echo "  make image       build a bootable disk image"
	@echo "  make check       fmt, clippy and the four-architecture checks"
	@echo "  make test        the workspace test suite"
	@echo "  make test-boot   the QEMU boot smoke test"
	@echo "  make test-gui    the QEMU framebuffer golden test"
	@echo "  make fmt         format the workspace"
	@echo "  make clippy      lint the workspace"
	@echo "  make clean       remove build output"
	@echo ""
	@echo "Variables: ARCH=$(ARCH) PROFILE=$(PROFILE) TIMEOUT=$(TIMEOUT)"

# The one command a new contributor needs: compile everything and show the
# desktop. `--display` opens QEMU's SDL window instead of running headless.
run:
	$(XTASK) run --arch $(ARCH) --profile $(PROFILE) --display --timeout $(TIMEOUT)

build:
	$(XTASK) build --arch $(ARCH) --profile $(PROFILE)

image:
	$(XTASK) image --arch $(ARCH)

fmt:
	$(CARGO) fmt --all

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

# `cargo check` per target rather than one `--target` sweep: the loader is a
# UEFI target and the userland binaries are bare-metal targets, so no single
# invocation covers all of them. These mirror CI's `cross-check` and
# `uefi-loader` jobs: the kernel crate proves `no_std` portability for the
# bare-metal targets, and the loader is checked on the UEFI target alone.
check: fmt-check clippy
	@echo "==> cargo check -p vela-kernel-entry --target x86_64-unknown-none"
	$(CARGO) check -p vela-kernel-entry --target x86_64-unknown-none || exit 1
	@for target in aarch64-unknown-none riscv64gc-unknown-none-elf loongarch64-unknown-none; do \
		echo "==> cargo check -p vela-hal-common --target $$target"; \
		$(CARGO) check -p vela-hal-common --target $$target || exit 1; \
	done
	@echo "==> cargo check -p vela-arch-aarch64 --target aarch64-unknown-none"
	$(CARGO) check -p vela-arch-aarch64 --target aarch64-unknown-none || exit 1
	@echo "==> cargo check -p vela-arch-riscv64 --target riscv64gc-unknown-none-elf"
	$(CARGO) check -p vela-arch-riscv64 --target riscv64gc-unknown-none-elf || exit 1
	@echo "==> cargo check -p vela-arch-loongarch64 --target loongarch64-unknown-none"
	$(CARGO) check -p vela-arch-loongarch64 --target loongarch64-unknown-none || exit 1
	@echo "==> cargo check -p vela-uefi-loader --features uefi --target x86_64-unknown-uefi"
	$(CARGO) check -p vela-uefi-loader --features uefi --target x86_64-unknown-uefi || exit 1

fmt-check:
	$(CARGO) fmt --all -- --check

# The whole workspace, including the hosted platform and SDK tests.
test:
	$(CARGO) test --workspace

test-boot:
	$(XTASK) test --suite boot-smoke

test-gui:
	$(XTASK) test --suite gui-golden

clean:
	$(CARGO) clean
