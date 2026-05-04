# Coffer Kernel Builder
# Builds a minimal Linux kernel for Firecracker MicroVMs.
#
# Required features:
#   - vsock (CONFIG_VIRTIO_VSOCKETS)
#   - EROFS (CONFIG_EROFS_FS)
#   - overlayfs (CONFIG_OVERLAY_FS)
#   - virtio-net, virtio-blk
#   - devtmpfs, tmpfs
#   - serial console
#
# Usage:
#   docker build --build-arg KERNEL_VERSION=5.10.225 -t coffer-kernel-build .
#   docker run --rm -v $(pwd)/output:/output coffer-kernel-build

FROM debian:bookworm-slim

ARG KERNEL_VERSION=5.10.225
ARG ARCH=x86_64
ARG JOBS=4

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential bc bison flex libssl-dev libelf-dev wget ca-certificates cpio kmod \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

RUN wget -q https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-${KERNEL_VERSION}.tar.xz \
    && tar -xf linux-${KERNEL_VERSION}.tar.xz \
    && rm linux-${KERNEL_VERSION}.tar.xz

WORKDIR /build/linux-${KERNEL_VERSION}

# Copy pre-downloaded Firecracker microvm kernel config.
COPY microvm-kernel-ci-x86_64-5.10.config .config

# Disable features that require build-time tools not in this image.
RUN ./scripts/config --disable CONFIG_BPF_PRELOAD

# Ensure required Coffer features are enabled.
RUN ./scripts/config \
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

# Build with silent oldconfig to accept defaults for any new options.
RUN yes "" | make oldconfig >/dev/null 2>&1 \
    && make -j${JOBS} vmlinux

# Copy result to mounted /output on run.
CMD ["sh", "-c", "cp /build/linux-5.10.225/vmlinux /output/vmlinux && chmod 644 /output/vmlinux"]
