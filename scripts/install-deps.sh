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
log_warn() { echo "[coffer-deps] WARNING: $*" >&2; }

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

    # Optional: skopeo (needed for OCI template builds)
    if ! command -v skopeo &>/dev/null; then
        sudo apt-get install -y -qq skopeo || log_warn "skopeo not available in apt (optional, needed for template builds)"
    fi
}

install_fedora() {
    log_info "Detected Fedora/RHEL family ($ID)."
    log_info "Installing dependencies via dnf..."
    sudo dnf install -y \
        gcc make bc bison flex \
        openssl-devel elfutils-libelf-devel \
        wget cpio kmod \
        erofs-utils lz4

    if ! command -v skopeo &>/dev/null; then
        sudo dnf install -y skopeo || log_warn "skopeo not available in dnf (optional, needed for template builds)"
    fi
    if ! command -v umoci &>/dev/null; then
        sudo dnf install -y umoci 2>/dev/null || true
    fi
}

install_arch() {
    log_info "Detected Arch Linux ($ID)."
    log_info "Installing dependencies via pacman..."
    sudo pacman -S --needed --noconfirm \
        base-devel bc bison flex \
        openssl libelf \
        wget cpio kmod \
        erofs-utils lz4

    if ! command -v skopeo &>/dev/null; then
        sudo pacman -S --needed --noconfirm skopeo 2>/dev/null || log_warn "skopeo not available in pacman (optional, needed for template builds)"
    fi
    if ! command -v umoci &>/dev/null; then
        sudo pacman -S --needed --noconfirm umoci 2>/dev/null || true
    fi
}

install_alpine() {
    log_info "Detected Alpine Linux ($ID)."
    log_info "Installing dependencies via apk..."
    sudo apk add --no-cache \
        build-base bc bison flex \
        openssl-dev elfutils-dev \
        wget cpio kmod \
        erofs-utils lz4

    if ! command -v skopeo &>/dev/null; then
        sudo apk add --no-cache skopeo 2>/dev/null || log_warn "skopeo not available in apk (optional, needed for template builds)"
    fi
}

install_opensuse() {
    log_info "Detected openSUSE ($ID)."
    log_info "Installing dependencies via zypper..."
    sudo zypper install -y \
        gcc make bc bison flex \
        libopenssl-devel libelf-devel \
        wget cpio kmod \
        erofs-utils lz4

    if ! command -v skopeo &>/dev/null; then
        sudo zypper install -y skopeo 2>/dev/null || log_warn "skopeo not available in zypper (optional, needed for template builds)"
    fi
    if ! command -v umoci &>/dev/null; then
        sudo zypper install -y umoci 2>/dev/null || true
    fi
}

# ------------------------------------------------------------------
# Install umoci from GitHub releases (static binary)
# ------------------------------------------------------------------
install_umoci() {
    if command -v umoci &>/dev/null; then
        log_info "umoci already installed ($(command -v umoci))."
        return 0
    fi

    local bindir="/usr/local/bin"
    if [ ! -w "$bindir" ] && [ ! -d "$bindir" ]; then
        bindir="$HOME/.local/bin"
        mkdir -p "$bindir"
    fi

    local arch
    case "$(uname -m)" in
        x86_64)  arch="amd64" ;;
        aarch64) arch="arm64" ;;
        *)       arch="$(uname -m)" ;;
    esac

    local version="v0.6.0"
    local url="https://github.com/opencontainers/umoci/releases/download/${version}/umoci.linux.${arch}"

    log_info "Downloading umoci ${version} (${arch}) to ${bindir}/umoci ..."
    if ! wget -q -O "${bindir}/umoci" "$url"; then
        log_warn "Failed to download umoci. Template builds will require manual installation:"
        log_warn "  https://github.com/opencontainers/umoci/releases"
        return 1
    fi
    chmod +x "${bindir}/umoci"
    log_info "umoci installed to ${bindir}/umoci"
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
        echo "  - Template build: skopeo umoci"
        echo ""
        echo "Then open an issue at https://github.com/agentlink-im/coffer/issues"
        echo "with your distribution name so we can add support."
        exit 1
        ;;
esac

# Install umoci from GitHub if not available in the distro packages
install_umoci || true

log_info "All dependencies installed successfully."
log_info "You can now run: make install"
