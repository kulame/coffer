#!/usr/bin/env bash
set -euo pipefail

# Coffer Dependency Installer
# Auto-detects the Linux distribution and installs build dependencies.
#
# Usage:
#   ./scripts/install-deps.sh
#   # or
#   make install-deps

log_info() { echo "[coffer-deps] $*"; }
log_error() { echo "[coffer-deps] ERROR: $*" >&2; }

# ------------------------------------------------------------------
# Detect package manager
# ------------------------------------------------------------------
detect_distro() {
    if [ -f /etc/os-release ]; then
        # shellcheck source=/dev/null
        . /etc/os-release
        echo "${ID:-unknown}"
        return
    elif command -v lsb_release &>/dev/null; then
        lsb_release -si | tr '[:upper:]' '[:lower:]'
        return
    else
        echo "unknown"
        return
    fi
}

# ------------------------------------------------------------------
# Install routines per distro
# ------------------------------------------------------------------
install_debian() {
    log_info "Detected Debian/Ubuntu family ($ID)."
    log_info "Installing dependencies via apt..."
    sudo apt-get update -qq
    sudo apt-get install -y -qq \
        build-essential bc bison flex \
        libssl-dev libelf-dev \
        wget cpio kmod \
        erofs-utils lz4
}

install_fedora() {
    log_info "Detected Fedora/RHEL family ($ID)."
    log_info "Installing dependencies via dnf..."
    sudo dnf install -y \
        gcc make bc bison flex \
        openssl-devel elfutils-libelf-devel \
        wget cpio kmod \
        erofs-utils lz4
}

install_arch() {
    log_info "Detected Arch Linux ($ID)."
    log_info "Installing dependencies via pacman..."
    sudo pacman -S --needed --noconfirm \
        base-devel bc bison flex \
        openssl libelf \
        wget cpio kmod \
        erofs-utils lz4
}

install_alpine() {
    log_info "Detected Alpine Linux ($ID)."
    log_info "Installing dependencies via apk..."
    sudo apk add --no-cache \
        build-base bc bison flex \
        openssl-dev elfutils-dev \
        wget cpio kmod \
        erofs-utils lz4
}

install_opensuse() {
    log_info "Detected openSUSE ($ID)."
    log_info "Installing dependencies via zypper..."
    sudo zypper install -y \
        gcc make bc bison flex \
        libopenssl-devel libelf-devel \
        wget cpio kmod \
        erofs-utils lz4
}

# ------------------------------------------------------------------
# Main
# ------------------------------------------------------------------
DISTRO=$(detect_distro)
ID="$DISTRO"
log_info "Detected distribution: $DISTRO"

case "$DISTRO" in
    debian|ubuntu|linuxmint|pop|elementary|zorin|kali)
        install_debian
        ;;
    fedora|rhel|centos|almalinux|rocky|oracle)
        install_fedora
        ;;
    arch|manjaro|endeavouros)
        install_arch
        ;;
    alpine)
        install_alpine
        ;;
    opensuse* | suse* | sles*)
        install_opensuse
        ;;
    *)
        log_error "Unsupported distribution: '$DISTRO'"
        echo ""
        echo "Please install the following packages manually:"
        echo "  - Kernel build:  gcc make bc bison flex libssl-dev libelf-dev wget cpio kmod"
        echo "  - Rootfs build:  erofs-utils lz4"
        echo ""
        echo "Then open an issue at https://github.com/agentlink-im/coffer/issues"
        echo "with your distribution name so we can add support."
        exit 1
        ;;
esac

log_info "All dependencies installed successfully."
log_info "You can now run: make kernel && make rootfs"
