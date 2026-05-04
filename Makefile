# Coffer — Makefile
# Zero-Docker build workflow. All tools run natively on the host.
#
# Prerequisites:
#   Kernel build:  gcc make bc bison flex libssl-dev libelf-dev wget cpio kmod
#   Rootfs build:  erofs-utils lz4
#   Rust:          cargo >= 1.78
#
# Quickstart:
#   make firecracker   # Download Firecracker + Jailer binaries
#   make kernel        # Build guest kernel
#   make rootfs        # Build minimal rootfs
#   make test          # Run all tests

KERNEL_VERSION ?= 5.10.225
ARCH ?= x86_64
JOBS ?= $(shell nproc 2>/dev/null || echo 4)

COFFER_HOME ?= $(HOME)/.coffer
TEMPLATE_DIR ?= $(COFFER_HOME)/templates
KERNEL_DIR ?= $(COFFER_HOME)/kernel
RUN_DIR ?= $(COFFER_HOME)/run

FIRECRACKER_VERSION ?= 1.7.0
FIRECRACKER_URL ?= https://github.com/firecracker-microvm/firecracker/releases/download/v$(FIRECRACKER_VERSION)/firecracker-v$(FIRECRACKER_VERSION)-$(ARCH).tgz

# Paths to native build scripts
SCRIPT_DIR ?= $(PWD)/scripts

.PHONY: all help check-deps firecracker kernel rootfs template test test-integration test-local build clean clean-all

all: help

help:
	@echo "Coffer — MicroVM runtime for AI Agents (zero-Docker)"
	@echo ""
	@echo "Targets:"
	@echo "  make install-deps  — Auto-install build dependencies for your distro"
	@echo "  make check-deps    — Verify host build tools are installed"
	@echo "  make firecracker   — Download Firecracker and Jailer binaries"
	@echo "  make kernel        — Build Firecracker-compatible Linux kernel"
	@echo "  make rootfs        — Build minimal rootfs with coffer-init"
	@echo "  make template      — Build a full template (rootfs + snapshot)"
	@echo "  make test          — Run unit + integration tests"
	@echo "  make check         — Run cargo check"
	@echo "  make build         — Build all crates in release mode"
	@echo "  make clean         — Clean build artifacts"

# ------------------------------------------------------------------
# Install dependencies (auto-detect distro)
# ------------------------------------------------------------------
install-deps:
	$(SCRIPT_DIR)/install-deps.sh

# ------------------------------------------------------------------
# Dependency check
# ------------------------------------------------------------------
check-deps:
	@echo "Checking host dependencies..."
	@command -v gcc >/dev/null 2>&1 || { echo "MISSING: gcc"; exit 1; }
	@command -v make >/dev/null 2>&1 || { echo "MISSING: make"; exit 1; }
	@command -v bc >/dev/null 2>&1 || { echo "MISSING: bc"; exit 1; }
	@command -v bison >/dev/null 2>&1 || { echo "MISSING: bison"; exit 1; }
	@command -v flex >/dev/null 2>&1 || { echo "MISSING: flex"; exit 1; }
	@command -v wget >/dev/null 2>&1 || { echo "MISSING: wget"; exit 1; }
	@command -v mkfs.erofs >/dev/null 2>&1 || { echo "MISSING: mkfs.erofs (install erofs-utils)"; exit 1; }
	@echo "All required tools found."

# ------------------------------------------------------------------
# Firecracker binary (downloaded directly)
# ------------------------------------------------------------------
firecracker:
	@echo "Downloading Firecracker v$(FIRECRACKER_VERSION)..."
	mkdir -p $(KERNEL_DIR)
	cd $(KERNEL_DIR) && curl -fsSL $(FIRECRACKER_URL) | tar -xz
	cp $(KERNEL_DIR)/release-v$(FIRECRACKER_VERSION)-$(ARCH)/firecracker-v$(FIRECRACKER_VERSION)-$(ARCH) $(KERNEL_DIR)/firecracker
	cp $(KERNEL_DIR)/release-v$(FIRECRACKER_VERSION)-$(ARCH)/jailer-v$(FIRECRACKER_VERSION)-$(ARCH) $(KERNEL_DIR)/jailer
	chmod +x $(KERNEL_DIR)/firecracker $(KERNEL_DIR)/jailer
	rm -rf $(KERNEL_DIR)/release-v$(FIRECRACKER_VERSION)-$(ARCH)
	@echo "Firecracker installed to $(KERNEL_DIR)/firecracker"

# ------------------------------------------------------------------
# Kernel build (native, no Docker)
# ------------------------------------------------------------------
kernel:
	@echo "Building kernel $(KERNEL_VERSION) natively..."
	KERNEL_VERSION=$(KERNEL_VERSION) \
	ARCH=$(ARCH) \
	JOBS=$(JOBS) \
	BUILD_DIR=$(KERNEL_DIR) \
	$(SCRIPT_DIR)/build-kernel-native.sh

# ------------------------------------------------------------------
# Rootfs build (native, no Docker)
# ------------------------------------------------------------------
rootfs:
	@echo "Building rootfs natively..."
	mkdir -p $(TEMPLATE_DIR)/alpine
	BUILD_DIR=$(TEMPLATE_DIR)/alpine \
	OUTPUT=$(TEMPLATE_DIR)/alpine/rootfs.erofs \
	AGENT_BIN=$(PWD)/rootfs-builder/coffer-agent \
	$(SCRIPT_DIR)/build-rootfs-native.sh

# ------------------------------------------------------------------
# Full template (rootfs + snapshot)
# ------------------------------------------------------------------
template: rootfs kernel firecracker
	@echo "Creating template snapshot..."
	cargo run --bin coffer-cli -- template build \
		--name alpine \
		--image docker.io/library/alpine:latest \
		--kernel $(KERNEL_DIR)/vmlinux

# ------------------------------------------------------------------
# Rust build & test
# ------------------------------------------------------------------
check:
	cargo check --workspace

build:
	cargo build --workspace --release

test:
	cargo test --workspace

test-integration:
	COFFER_TEST_FIRECRACKER_PATH=$(KERNEL_DIR)/firecracker \
	COFFER_TEST_KERNEL_PATH=$(KERNEL_DIR)/vmlinux \
	COFFER_TEST_ROOTFS_PATH=$(TEMPLATE_DIR)/alpine/rootfs.erofs \
	cargo test --test integration -- --ignored

test-local:
	COFFER_TEST_FIRECRACKER_PATH=$(PWD)/kernel/build/firecracker \
	COFFER_TEST_KERNEL_PATH=$(PWD)/kernel/build/vmlinux \
	COFFER_TEST_ROOTFS_PATH=$(PWD)/rootfs-builder/output/rootfs.erofs \
	cargo test --test integration -- --ignored

# ------------------------------------------------------------------
# Cleanup
# ------------------------------------------------------------------
clean:
	cargo clean
	rm -rf $(RUN_DIR)

clean-all: clean
	rm -rf $(KERNEL_DIR) $(TEMPLATE_DIR)
