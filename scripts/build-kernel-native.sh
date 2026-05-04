#!/usr/bin/env bash
set -euo pipefail

# Coffer Native Kernel Builder
# Builds a minimal Firecracker-compatible Linux kernel on the host.
# No Docker required.
#
# Prerequisites (Debian/Ubuntu):
#   sudo apt install build-essential bc bison flex libssl-dev libelf-dev wget cpio kmod
#
# Prerequisites (Fedora/RHEL):
#   sudo dnf install gcc make bc bison flex openssl-devel elfutils-libelf-devel wget cpio kmod

KERNEL_VERSION="${KERNEL_VERSION:-5.10.225}"
ARCH="${ARCH:-x86_64}"
JOBS="${JOBS:-$(nproc 2>/dev/null || echo 4)}"
BUILD_DIR="${BUILD_DIR:-$(pwd)/build}"

# ------------------------------------------------------------------
# Dependency checks
# ------------------------------------------------------------------
check_cmd() {
    if ! command -v "$1" &>/dev/null; then
        echo "ERROR: Required tool '$1' is not installed."
        echo ""
        echo "Install on Debian/Ubuntu:"
        echo "  sudo apt install build-essential bc bison flex libssl-dev libelf-dev wget cpio kmod"
        echo ""
        echo "Install on Fedora/RHEL:"
        echo "  sudo dnf install gcc make bc bison flex openssl-devel elfutils-libelf-devel wget cpio kmod"
        exit 1
    fi
}

echo "Checking dependencies..."
check_cmd gcc
check_cmd make
check_cmd bc
check_cmd bison
check_cmd flex
check_cmd wget

# Check kernel headers / libraries indirectly by trying compilation
if ! ld -lssl --help &>/dev/null && [ ! -f /usr/include/openssl/ssl.h ]; then
    echo "WARNING: OpenSSL development headers may be missing."
fi
if [ ! -d /usr/include/elfutils ] && [ ! -f /usr/include/libelf.h ]; then
    echo "WARNING: libelf development headers may be missing."
fi

mkdir -p "$BUILD_DIR"
KERNEL_SRC="$BUILD_DIR/linux-$KERNEL_VERSION"

# ------------------------------------------------------------------
# Download kernel source
# ------------------------------------------------------------------
if [ ! -d "$KERNEL_SRC" ]; then
    echo "Downloading Linux kernel $KERNEL_VERSION..."
    cd "$BUILD_DIR"
    wget -q "https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-${KERNEL_VERSION}.tar.xz"
    echo "Extracting..."
    tar -xf "linux-${KERNEL_VERSION}.tar.xz"
    rm "linux-${KERNEL_VERSION}.tar.xz"
    cd - >/dev/null
fi

cd "$KERNEL_SRC"

# ------------------------------------------------------------------
# Download or copy Firecracker microvm config
# ------------------------------------------------------------------
CONFIG_URL="https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-x86_64-5.10.config"
CONFIG_FILE="$BUILD_DIR/microvm-kernel-ci-x86_64-5.10.config"

if [ ! -f "$CONFIG_FILE" ]; then
    echo "Downloading Firecracker microvm kernel config..."
    if ! wget -q -O "$CONFIG_FILE" "$CONFIG_URL"; then
        echo "WARNING: Failed to download Firecracker config from:"
        echo "  $CONFIG_URL"
        echo "Falling back to 'make defconfig' (larger kernel, but guaranteed to compile)."
        make defconfig >/dev/null 2>&1
    else
        cp "$CONFIG_FILE" .config
    fi
else
    cp "$CONFIG_FILE" .config
fi

# ------------------------------------------------------------------
# Apply Coffer-specific options
# ------------------------------------------------------------------
echo "Applying Coffer kernel options..."
./scripts/config \
    --disable CONFIG_BPF_PRELOAD \
    --enable CONFIG_VIRTIO_VSOCKETS \
    --enable CONFIG_VIRTIO_VSOCKETS_COMMON \
    --enable CONFIG_VHOST_VSOCK \
    --enable CONFIG_EROFS_FS \
    --enable CONFIG_EROFS_FS_ZIP \
    --enable CONFIG_EROFS_FS_ZIP_LZ4 \
    --enable CONFIG_OVERLAY_FS \
    --enable CONFIG_TMPFS \
    --enable CONFIG_DEVTMPFS \
    --enable CONFIG_DEVTMPFS_MOUNT \
    --enable CONFIG_SERIAL_8250 \
    --enable CONFIG_SERIAL_8250_CONSOLE \
    --enable CONFIG_VIRTIO_CONSOLE \
    --enable CONFIG_PCI \
    --enable CONFIG_VIRTIO_PCI \
    --enable CONFIG_BLK_MQ_PCI \
    --enable CONFIG_EXT4_FS \
    --enable CONFIG_IPV6 \
    --enable CONFIG_UNIX \
    --enable CONFIG_INET \
    --enable CONFIG_NET \
    --enable CONFIG_VIRTIO_NET \
    --enable CONFIG_VIRTIO_BLK \
    --enable CONFIG_BLOCK \
    --enable CONFIG_BLK_MQ \
    --enable CONFIG_TTY \
    --enable CONFIG_UNIX98_PTYS \
    --enable CONFIG_POSIX_MQUEUE \
    --enable CONFIG_NAMESPACES \
    --enable CONFIG_UTS_NS \
    --enable CONFIG_IPC_NS \
    --enable CONFIG_PID_NS \
    --enable CONFIG_NET_NS \
    --enable CONFIG_CGROUPS \
    --enable CONFIG_SECCOMP \
    --enable CONFIG_SECCOMP_FILTER \
    --enable CONFIG_PRINTK \
    --enable CONFIG_PRINTK_TIME \
    --enable CONFIG_BINFMT_ELF \
    --enable CONFIG_BINFMT_SCRIPT \
    --enable CONFIG_MODULES \
    --enable CONFIG_MODULE_UNLOAD

# Accept defaults for any new options introduced by the config changes.
echo "Running make olddefconfig..."
yes "" | make oldconfig >/dev/null 2>&1

# ------------------------------------------------------------------
# Build
# ------------------------------------------------------------------
echo "Building kernel with $JOBS parallel jobs..."
make -j"$JOBS" vmlinux

echo ""
echo "========================================"
echo "Kernel build complete!"
echo "  Output: $KERNEL_SRC/vmlinux"
ls -lh "$KERNEL_SRC/vmlinux"
echo "========================================"

# Optionally copy to a standard location
STANDARD_KERNEL="$BUILD_DIR/vmlinux"
cp "$KERNEL_SRC/vmlinux" "$STANDARD_KERNEL"
echo "Copied to: $STANDARD_KERNEL"
