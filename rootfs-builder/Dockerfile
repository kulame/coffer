FROM alpine:3.19

RUN apk add --no-cache erofs-utils lz4 curl ca-certificates

COPY build.sh /rootfs-builder/build.sh
RUN chmod +x /rootfs-builder/build.sh

# Optional: if coffer-agent binary exists in build context, copy it.
# Build will gracefully handle absence.
CMD ["/rootfs-builder/build.sh"]
