#!/bin/sh
# Build a minimal Alpine rootfs with coffer-init overlay support.
# Outputs /output/rootfs.erofs

set -e

ROOTFS=/tmp/rootfs
OUTPUT=/output

mkdir -p "$ROOTFS" "$OUTPUT"

# ------------------------------------------------------------------
# 1. Bootstrap minimal Alpine rootfs using apk
# ------------------------------------------------------------------
apk add --no-cache --root "$ROOTFS" \
    alpine-base \
    busybox \
    musl \
    ca-certificates \
    curl \
    2>/dev/null || true

# Clean package cache
rm -rf "$ROOTFS/var/cache/apk"/*

# ------------------------------------------------------------------
# 2. Create essential directories and device nodes
# ------------------------------------------------------------------
mkdir -p "$ROOTFS"/dev "$ROOTFS"/proc "$ROOTFS"/sys "$ROOTFS"/run "$ROOTFS"/tmp
mkdir -p "$ROOTFS"/etc "$ROOTFS"/sbin "$ROOTFS"/bin "$ROOTFS"/usr/bin "$ROOTFS"/usr/local/bin

mknod -m 666 "$ROOTFS"/dev/null    c 1 3  2>/dev/null || true
mknod -m 666 "$ROOTFS"/dev/zero    c 1 5  2>/dev/null || true
mknod -m 666 "$ROOTFS"/dev/random  c 1 8  2>/dev/null || true
mknod -m 666 "$ROOTFS"/dev/urandom c 1 9  2>/dev/null || true
mknod -m 666 "$ROOTFS"/dev/tty     c 5 0  2>/dev/null || true
mknod -m 666 "$ROOTFS"/dev/console c 5 1  2>/dev/null || true

ln -sf /proc/self/fd   "$ROOTFS"/dev/fd     2>/dev/null || true
ln -sf /proc/self/fd/0 "$ROOTFS"/dev/stdin  2>/dev/null || true
ln -sf /proc/self/fd/1 "$ROOTFS"/dev/stdout 2>/dev/null || true
ln -sf /proc/self/fd/2 "$ROOTFS"/dev/stderr 2>/dev/null || true

# ------------------------------------------------------------------
# 3. Write coffer-init (overlay rootfs setup)
# ------------------------------------------------------------------
cat > "$ROOTFS"/sbin/coffer-init << 'INITEOF'
#!/bin/sh
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

exec chroot . /sbin/init "$@"
INITEOF
chmod +x "$ROOTFS"/sbin/coffer-init

# ------------------------------------------------------------------
# 4. Write inittab (busybox init) that delegates to coffer-init
# ------------------------------------------------------------------
cat > "$ROOTFS"/etc/inittab << 'EOF'
::sysinit:/sbin/coffer-init
::respawn:/sbin/getty -L ttyS0 115200 vt100
::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/poweroff
EOF

# ------------------------------------------------------------------
# 5. Write fstab and basic config
# ------------------------------------------------------------------
cat > "$ROOTFS"/etc/fstab << 'EOF'
proc  /proc proc  defaults 0 0
sysfs /sys  sysfs defaults 0 0
tmpfs /tmp  tmpfs defaults,size=64m 0 0
tmpfs /run  tmpfs defaults,size=32m 0 0
EOF

echo "root:x:0:0:root:/root:/bin/sh" > "$ROOTFS"/etc/passwd
echo "root:x:0:" > "$ROOTFS"/etc/group
echo "nameserver 8.8.8.8" > "$ROOTFS"/etc/resolv.conf

# ------------------------------------------------------------------
# 6. Inject coffer-agent binary if present
# ------------------------------------------------------------------
if [ -f /rootfs-builder/coffer-agent ]; then
    cp /rootfs-builder/coffer-agent "$ROOTFS"/usr/local/bin/coffer-agent
    chmod +x "$ROOTFS"/usr/local/bin/coffer-agent
    echo "coffer-agent injected"
fi

# ------------------------------------------------------------------
# 7. Build EROFS image
# ------------------------------------------------------------------
mkfs.erofs -zlz4 "$OUTPUT"/rootfs.erofs "$ROOTFS"

echo "Rootfs built: $OUTPUT/rootfs.erofs"
ls -lh "$OUTPUT"/rootfs.erofs
