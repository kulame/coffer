#!/usr/bin/env bash
set -euo pipefail

# Coffer Native Rootfs Builder
# Creates a minimal Alpine-based EROFS rootfs on the host.
# No Docker required.
#
# Prerequisites:
#   - erofs-utils (mkfs.erofs)
#   - lz4 (optional, for lz4 compression)
#
# Install on Debian/Ubuntu:
#   sudo apt install erofs-utils lz4
# Install on Fedora/RHEL:
#   sudo dnf install erofs-utils lz4
# Install on Arch:
#   sudo pacman -S erofs-utils lz4

BUILD_DIR="${BUILD_DIR:-$(pwd)/build}"
ROOTFS_DIR="$BUILD_DIR/rootfs-work"
OUTPUT="${OUTPUT:-$BUILD_DIR/rootfs.erofs}"
AGENT_BIN="${AGENT_BIN:-}"

# ------------------------------------------------------------------
# Dependency checks
# ------------------------------------------------------------------
check_cmd() {
    if ! command -v "$1" &>/dev/null; then
        echo "ERROR: Required tool '$1' is not installed."
        echo ""
        echo "Install on Debian/Ubuntu:"
        echo "  sudo apt install erofs-utils lz4"
        echo ""
        echo "Install on Fedora/RHEL:"
        echo "  sudo dnf install erofs-utils lz4"
        echo ""
        echo "Install on Arch:"
        echo "  sudo pacman -S erofs-utils lz4"
        exit 1
    fi
}

echo "Checking dependencies..."
check_cmd mkfs.erofs

# ------------------------------------------------------------------
# Create minimal rootfs tree
# ------------------------------------------------------------------
echo "Creating rootfs tree at $ROOTFS_DIR..."
rm -rf "$ROOTFS_DIR"
mkdir -p "$ROOTFS_DIR"/{bin,sbin,dev,proc,sys,tmp,run,etc,usr/bin,usr/local/bin}

# Create essential device nodes
[ -e "$ROOTFS_DIR/dev/null" ] || mknod -m 666 "$ROOTFS_DIR/dev/null" c 1 3
[ -e "$ROOTFS_DIR/dev/zero" ] || mknod -m 666 "$ROOTFS_DIR/dev/zero" c 1 5
[ -e "$ROOTFS_DIR/dev/random" ] || mknod -m 666 "$ROOTFS_DIR/dev/random" c 1 8
[ -e "$ROOTFS_DIR/dev/urandom" ] || mknod -m 666 "$ROOTFS_DIR/dev/urandom" c 1 9
[ -e "$ROOTFS_DIR/dev/tty" ] || mknod -m 666 "$ROOTFS_DIR/dev/tty" c 5 0

# Symlinks for convenience
ln -sf /proc/self/fd "$ROOTFS_DIR/dev/fd"
ln -sf /proc/self/fd/0 "$ROOTFS_DIR/dev/stdin"
ln -sf /proc/self/fd/1 "$ROOTFS_DIR/dev/stdout"
ln -sf /proc/self/fd/2 "$ROOTFS_DIR/dev/stderr"

# Create coffer-init (PID 1 inside guest)
cat > "$ROOTFS_DIR/sbin/coffer-init" <<'INIT'
#!/bin/sh
# Coffer guest init — mounts overlayfs on top of EROFS rootfs

mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null

mkdir -p /newroot/overlay/upper /newroot/overlay/work
mount -t tmpfs -o size=128M tmpfs /newroot/overlay
mount -t overlay overlay \
  -o lowerdir=/,upperdir=/newroot/overlay/upper,workdir=/newroot/overlay/work \
  /newroot

cd /newroot
mkdir -p oldroot
pivot_root . oldroot
mount --move /oldroot/proc /proc 2>/dev/null
mount --move /oldroot/sys /sys 2>/dev/null
mount --move /oldroot/dev /dev 2>/dev/null

# If coffer-agent exists, start it; otherwise drop to a shell.
if [ -x /usr/local/bin/coffer-agent ]; then
    exec /usr/local/bin/coffer-agent
elif [ -x /sbin/init ]; then
    exec chroot . /sbin/init "$@"
else
    echo "No init found. Dropping to shell."
    exec /bin/sh
fi
INIT
chmod +x "$ROOTFS_DIR/sbin/coffer-init"

# Minimal passwd/group for sanity
echo "root:x:0:0:root:/root:/bin/sh" > "$ROOTFS_DIR/etc/passwd"
echo "root:x:0:" > "$ROOTFS_DIR/etc/group"

# Copy coffer-agent if provided
if [ -n "$AGENT_BIN" ] && [ -f "$AGENT_BIN" ]; then
    echo "Copying coffer-agent into rootfs..."
    cp "$AGENT_BIN" "$ROOTFS_DIR/usr/local/bin/coffer-agent"
    chmod +x "$ROOTFS_DIR/usr/local/bin/coffer-agent"
else
    echo "WARNING: coffer-agent binary not provided (AGENT_BIN='$AGENT_BIN')."
    echo "         The guest will drop to a shell instead."
fi

# ------------------------------------------------------------------
# Build EROFS image
# ------------------------------------------------------------------
echo "Building EROFS image: $OUTPUT"
mkdir -p "$(dirname "$OUTPUT")"

# Use lz4 compression if available, otherwise fall back to lz4hc or no compression.
if mkfs.erofs --help 2>&1 | grep -q '\-zlz4'; then
    mkfs.erofs -zlz4 "$OUTPUT" "$ROOTFS_DIR"
else
    mkfs.erofs "$OUTPUT" "$ROOTFS_DIR"
fi

echo ""
echo "========================================"
echo "Rootfs build complete!"
ls -lh "$OUTPUT"
echo "========================================"

# Clean up work directory
rm -rf "$ROOTFS_DIR"
