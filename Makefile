SHELL := /bin/sh

ROOT := $(CURDIR)

.PHONY: help setup dev-env check test components kernel initramfs image iso build qemu boot-test clean

help:
	@printf '%s\n' \
	  'AgentOS v0.1.0-dev build targets:' \
	  '  make setup      Install Debian/Ubuntu host prerequisites' \
	  '  make dev-env    Validate the local Rust/JSON development environment' \
	  '  make check      Run Rust checks and shell validation' \
	  '  make test       Run unit, schema, and dev-service integration tests' \
	  '  make components Build all AgentOS Rust binaries' \
	  '  make kernel     Build the pinned Linux kernel (or use AGENTOS_KERNEL_IMAGE)' \
	  '  make image      Run the explicit kernel/userspace/ISO pipeline' \
	  '  make iso        Build build/agentos.iso (UEFI + legacy BIOS)' \
	  '  make build      Alias for make iso' \
	  '  make qemu       Boot the ISO with a visible QEMU window + serial console' \
	  '  make boot-test  Rebuild with autotest and run headless QEMU smoke test' \
	  '  make clean      Remove generated build and Cargo output'

setup:
	@./scripts/setup-deps.sh

dev-env:
	@./scripts/dev-env.sh

check:
	@./scripts/check.sh

test:
	@./scripts/test.sh

components:
	@./scripts/build-components.sh

kernel:
	@./scripts/build-kernel.sh

initramfs:
	@./scripts/build-initramfs.sh

image:
	@./scripts/build.sh

iso:
	@./scripts/build-iso.sh

build: iso

qemu:
	@./scripts/qemu.sh

boot-test:
	@./scripts/boot-test.sh

clean:
	@rm -rf build target
