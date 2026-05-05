# Coffer — Makefile
# Zero-Docker build workflow. All tools run natively on the host.
#
# Prerequisites:
#   Kernel build:  gcc make bc bison flex libssl-dev libelf-dev wget cpio kmod
#   Rootfs build:  erofs-utils lz4
#   Rust:          cargo >= 1.78
#
# Quickstart:
#   make install       # Full installation (deps + build + binaries + template)
#   make install-deps  # Install system dependencies
#   make firecracker   # Download Firecracker + Jailer binaries
#   make kernel        # Build guest kernel
#   make rootfs        # Build minimal rootfs
#   make test          # Run all tests

KERNEL_VERSION ?= 5.10.225
ARCH ?= x86_64
JOBS ?= $(shell nproc 2>/dev/null || echo 4)

# Installation paths
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin

# Coffer data home
COFFER_HOME ?= $(HOME)/.coffer
TEMPLATE_DIR ?= $(COFFER_HOME)/templates
KERNEL_DIR ?= $(COFFER_HOME)/kernel
RUN_DIR ?= $(COFFER_HOME)/run

FIRECRACKER_VERSION ?= 1.7.0
FIRECRACKER_URL ?= https://github.com/firecracker-microvm/firecracker/releases/download/v$(FIRECRACKER_VERSION)/firecracker-v$(FIRECRACKER_VERSION)-$(ARCH).tgz

# Paths to native build scripts
SCRIPT_DIR ?= $(PWD)/scripts

# Local pre-built artifacts
LOCAL_FIRECRACKER ?= kernel/build/firecracker
LOCAL_JAILER ?= kernel/build/jailer
LOCAL_VMLINUX ?= kernel/build/vmlinux
LOCAL_AGENT ?= target/release/coffer-agent
LOCAL_ROOTFS ?= rootfs-builder/output/rootfs.erofs

.PHONY: all help install uninstall install-deps check-deps \
        firecracker kernel rootfs agent build \
        install-bin install-data install-template verify-data \
        template test test-integration test-local check clean clean-all

# ===================================================================
# Help
# ===================================================================
all: help

help:
	@echo "Coffer — MicroVM runtime for AI Agents (zero-Docker)"
	@echo ""
	@echo "Installation:"
	@echo "  make install        — Full install: deps, build, binaries, template"
	@echo "  make install-deps   — Install system build dependencies"
	@echo "  make uninstall      — Remove installed binary and data"
	@echo ""
	@echo "Build targets:"
	@echo "  make build          — Build all Rust crates (release)"
	@echo "  make firecracker    — Download Firecracker and Jailer"
	@echo "  make kernel         — Build guest kernel"
	@echo "  make rootfs         — Build minimal rootfs"
	@echo "  make template       — Build default alpine template"
	@echo ""
	@echo "Development:"
	@echo "  make check          — cargo check"
	@echo "  make test           — Run unit tests"
	@echo "  make test-local     — Run integration tests with local artifacts"
	@echo "  make clean          — Clean build artifacts"
	@echo "  make clean-all      — Clean everything including data"

# ===================================================================
# Full installation
# ===================================================================
install: install-deps build install-bin verify-data install-template
	@echo ""
	@echo "========================================"
	@echo "✓ Coffer installed successfully!"
	@echo "========================================"
	@echo ""
	@echo "  CLI binary:  $(BINDIR)/coffer-cli"
	@echo "  Data home:   $(COFFER_HOME)"
	@echo ""
	@echo "Quick start:"
	@echo "  coffer-cli check"
	@echo "  coffer-cli template list"
	@echo "  coffer-cli run --template alpine -- echo hello"
	@echo ""
	@if [ ! -f "$(TEMPLATE_DIR)/alpine/snapshot.state" ]; then \
		echo "⚠ Template snapshot was not created (may require root for KVM)."; \
		echo "  To create it manually, run:"; \
		echo "    sudo $$(id -un):$$(id -gn) coffer-cli template build --name alpine docker.io/library/alpine:latest"; \
		echo "  Or simply run 'make template' with appropriate privileges."; \
	fi

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/coffer-cli
	rm -rf $(COFFER_HOME)
	@echo "✓ Coffer uninstalled."

# ===================================================================
# Install sub-steps
# ===================================================================
install-bin: build
	@install -d $(DESTDIR)$(BINDIR)
	install -m 755 target/release/coffer-cli $(DESTDIR)$(BINDIR)/coffer-cli
	@echo "✓ Installed coffer-cli -> $(DESTDIR)$(BINDIR)/coffer-cli"

verify-data:
	@echo "Verifying runtime data..."
	@install -d $(DESTDIR)$(KERNEL_DIR)
	@install -d $(DESTDIR)$(COFFER_HOME)/bin
	@install -d $(DESTDIR)$(TEMPLATE_DIR)
	@install -d $(DESTDIR)$(RUN_DIR)
	@install -d $(DESTDIR)$(RUN_DIR)/vsock

# Firecracker
	@if [ ! -f "$(DESTDIR)$(KERNEL_DIR)/firecracker" ]; then \
		if [ -f $(LOCAL_FIRECRACKER) ]; then \
			install -m 755 $(LOCAL_FIRECRACKER) $(DESTDIR)$(KERNEL_DIR)/firecracker; \
			echo "✓ Installed firecracker (local build) -> $(KERNEL_DIR)/firecracker"; \
		else \
			echo "→ Downloading firecracker..."; \
			$(MAKE) firecracker; \
			echo "✓ Firecracker downloaded -> $(KERNEL_DIR)/firecracker"; \
		fi \
	else \
		echo "✓ firecracker already present"; \
	fi

# Jailer
	@if [ ! -f "$(DESTDIR)$(KERNEL_DIR)/jailer" ]; then \
		if [ -f $(LOCAL_JAILER) ]; then \
			install -m 755 $(LOCAL_JAILER) $(DESTDIR)$(KERNEL_DIR)/jailer; \
			echo "✓ Installed jailer (local build) -> $(KERNEL_DIR)/jailer"; \
		else \
			echo "⚠ jailer not found (optional)"; \
		fi \
	else \
		echo "✓ jailer already present"; \
	fi

# Kernel
	@if [ ! -f "$(DESTDIR)$(KERNEL_DIR)/vmlinux" ]; then \
		if [ -f $(LOCAL_VMLINUX) ]; then \
			install -m 644 $(LOCAL_VMLINUX) $(DESTDIR)$(KERNEL_DIR)/vmlinux; \
			echo "✓ Installed kernel (local build) -> $(KERNEL_DIR)/vmlinux"; \
		else \
			echo "→ Building kernel..."; \
			$(MAKE) kernel; \
			echo "✓ Kernel built -> $(KERNEL_DIR)/vmlinux"; \
		fi \
	else \
		echo "✓ kernel already present"; \
	fi

# Agent
	@if [ ! -f "$(DESTDIR)$(COFFER_HOME)/bin/coffer-agent" ]; then \
		if [ -f $(LOCAL_AGENT) ]; then \
			install -m 755 $(LOCAL_AGENT) $(DESTDIR)$(COFFER_HOME)/bin/coffer-agent; \
			echo "✓ Installed coffer-agent (built) -> $(COFFER_HOME)/bin/coffer-agent"; \
		elif [ -f rootfs-builder/coffer-agent ]; then \
			install -m 755 rootfs-builder/coffer-agent $(DESTDIR)$(COFFER_HOME)/bin/coffer-agent; \
			echo "✓ Installed coffer-agent (pre-built) -> $(COFFER_HOME)/bin/coffer-agent"; \
		else \
			echo "⚠ coffer-agent not found. Run 'make build' first."; \
		fi \
	else \
		echo "✓ coffer-agent already present"; \
	fi

install-template: verify-data
	@echo ""
	@echo "Building default alpine template..."
	@if [ -f "$(BINDIR)/coffer-cli" ]; then \
		CLI="$(BINDIR)/coffer-cli"; \
	else \
		CLI="cargo run --bin coffer-cli --release --"; \
	fi; \
	COFFER_FIRECRACKER_PATH=$(KERNEL_DIR)/firecracker \
	COFFER_KERNEL_PATH=$(KERNEL_DIR)/vmlinux \
	COFFER_AGENT_BIN=$(COFFER_HOME)/bin/coffer-agent \
	$$CLI template build --name alpine docker.io/library/alpine:latest || \
	(echo "⚠ Template build failed (may require root for KVM / network setup)." && exit 0)

# ===================================================================
# Install dependencies (auto-detect distro)
# ===================================================================
install-deps:
	$(SCRIPT_DIR)/install-deps.sh

# ===================================================================
# Dependency check
# ===================================================================
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

# ===================================================================
# Firecracker binary (downloaded directly)
# ===================================================================
firecracker:
	@echo "Downloading Firecracker v$(FIRECRACKER_VERSION)..."
	@mkdir -p $(KERNEL_DIR)
	cd $(KERNEL_DIR) && curl -fsSL $(FIRECRACKER_URL) | tar -xz
	cp $(KERNEL_DIR)/release-v$(FIRECRACKER_VERSION)-$(ARCH)/firecracker-v$(FIRECRACKER_VERSION)-$(ARCH) $(KERNEL_DIR)/firecracker
	cp $(KERNEL_DIR)/release-v$(FIRECRACKER_VERSION)-$(ARCH)/jailer-v$(FIRECRACKER_VERSION)-$(ARCH) $(KERNEL_DIR)/jailer
	chmod +x $(KERNEL_DIR)/firecracker $(KERNEL_DIR)/jailer
	rm -rf $(KERNEL_DIR)/release-v$(FIRECRACKER_VERSION)-$(ARCH)
	@echo "✓ Firecracker installed to $(KERNEL_DIR)/firecracker"

# ===================================================================
# Kernel build (native, no Docker)
# ===================================================================
kernel:
	@echo "Building kernel $(KERNEL_VERSION) natively..."
	@mkdir -p $(KERNEL_DIR)
	KERNEL_VERSION=$(KERNEL_VERSION) \
	ARCH=$(ARCH) \
	JOBS=$(JOBS) \
	BUILD_DIR=$(KERNEL_DIR) \
	$(SCRIPT_DIR)/build-kernel-native.sh

# ===================================================================
# Rootfs build (native, no Docker)
# ===================================================================
rootfs:
	@echo "Building rootfs natively..."
	@mkdir -p $(TEMPLATE_DIR)/alpine
	BUILD_DIR=$(TEMPLATE_DIR)/alpine \
	OUTPUT=$(TEMPLATE_DIR)/alpine/rootfs.erofs \
	AGENT_BIN=$(PWD)/rootfs-builder/coffer-agent \
	$(SCRIPT_DIR)/build-rootfs-native.sh

# ===================================================================
# Full template (rootfs + snapshot)
# ===================================================================
template: rootfs kernel firecracker build
	@echo "Creating template snapshot..."
	@if [ -f "$(BINDIR)/coffer-cli" ]; then \
		CLI="$(BINDIR)/coffer-cli"; \
	else \
		CLI="cargo run --bin coffer-cli --release --"; \
	fi; \
	COFFER_FIRECRACKER_PATH=$(KERNEL_DIR)/firecracker \
	COFFER_KERNEL_PATH=$(KERNEL_DIR)/vmlinux \
	COFFER_AGENT_BIN=$(COFFER_HOME)/bin/coffer-agent \
	$$CLI template build --name alpine docker.io/library/alpine:latest

# ===================================================================
# Rust build & test
# ===================================================================
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

# ===================================================================
# Cleanup
# ===================================================================
clean:
	cargo clean
	rm -rf $(RUN_DIR)

clean-all: clean
	rm -rf $(KERNEL_DIR) $(TEMPLATE_DIR)
