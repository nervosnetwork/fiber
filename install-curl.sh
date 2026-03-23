#!/bin/bash
# Fiber Network Node (FNN) Install Script
# Usage: curl -sSfL https://your-domain.com/install.sh | sh
# Or: curl -sSfL https://your-domain.com/install.sh | sh -s -- --help

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
FNN_VERSION="${FNN_VERSION:-0.7.1}"
CKB_CLI_VERSION="${CKB_CLI_VERSION:-1.12.0}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.fiber}"
NETWORK="${NETWORK:-testnet}"
GITHUB_RELEASE_URL="https://github.com/nervosnetwork/fiber/releases/download/v${FNN_VERSION}"
CKB_CLI_RELEASE_URL="https://github.com/nervosnetwork/ckb-cli/releases/download/v${CKB_CLI_VERSION}"

# Check if running in interactive mode
if [ -t 0 ]; then
    IS_INTERACTIVE=true
else
    IS_INTERACTIVE=false
fi

# Platform detection
PLATFORM=$(uname -s)
ARCH=$(uname -m)

case "$PLATFORM" in
    Linux*)
        case "$ARCH" in
            x86_64) BINARY_NAME="fnn_v${FNN_VERSION}-x86_64-linux-portable.tar.gz" ;;
            aarch64) BINARY_NAME="fnn_v${FNN_VERSION}-aarch64-linux-portable.tar.gz" ;;
            *) echo -e "${RED}Unsupported architecture: $ARCH${NC}"; exit 1 ;;
        esac
        ;;
    Darwin*)
        case "$ARCH" in
            x86_64) BINARY_NAME="fnn_v${FNN_VERSION}-x86_64-darwin-portable.tar.gz" ;;
            arm64) BINARY_NAME="fnn_v${FNN_VERSION}-aarch64-darwin-portable.tar.gz" ;;
            *) echo -e "${RED}Unsupported architecture: $ARCH${NC}"; exit 1 ;;
        esac
        ;;
    *)
        echo -e "${RED}Unsupported platform: $PLATFORM${NC}"
        exit 1
        ;;
esac

# Print helpers
print_header() {
    echo -e "${BLUE}"
    echo "=========================================="
    echo "    Fiber Network Node (FNN) Installer"
    echo "=========================================="
    echo -e "${NC}"
}

print_success() { echo -e "${GREEN}✓ $1${NC}"; }
print_warning() { echo -e "${YELLOW}⚠ $1${NC}"; }
print_error() { echo -e "${RED}✗ $1${NC}"; }
print_info() { echo -e "${BLUE}ℹ $1${NC}"; }

check_command() {
    command -v "$1" &> /dev/null
}

download_file() {
    local url="$1"
    local output="$2"
    
    if check_command curl; then
        curl -fsSL "$url" -o "$output" --progress-bar
    elif check_command wget; then
        wget -q --show-progress "$url" -O "$output"
    else
        print_error "Neither curl nor wget is installed"
        exit 1
    fi
}

install_fn() {
    print_header
    
    # Non-interactive mode uses defaults
    if [ "$IS_INTERACTIVE" = false ]; then
        print_info "Running in non-interactive mode with defaults"
        print_info "Install directory: $INSTALL_DIR"
        print_info "Network: $NETWORK"
        print_info "Version: $FNN_VERSION"
        echo ""
    fi
    
    # Create install directory
    mkdir -p "$INSTALL_DIR"
    
    # Download FNN binary
    print_info "Downloading FNN v${FNN_VERSION}..."
    local download_url="${GITHUB_RELEASE_URL}/${BINARY_NAME}"
    local temp_dir=$(mktemp -d)
    
    if ! download_file "$download_url" "${temp_dir}/${BINARY_NAME}"; then
        print_error "Failed to download FNN binary"
        rm -rf "$temp_dir"
        exit 1
    fi
    
    # Extract
    tar -xzf "${temp_dir}/${BINARY_NAME}" -C "$temp_dir"
    local fnn_binary=$(find "$temp_dir" -name "fnn" -type f | head -1)
    
    if [ -z "$fnn_binary" ]; then
        print_error "Could not find fnn binary in archive"
        rm -rf "$temp_dir"
        exit 1
    fi
    
    chmod +x "$fnn_binary"
    cp "$fnn_binary" "$INSTALL_DIR/"
    rm -rf "$temp_dir"
    print_success "FNN installed to $INSTALL_DIR/fnn"
    
    # Download config
    print_info "Downloading configuration..."
    local config_url="https://raw.githubusercontent.com/nervosnetwork/fiber/v${FNN_VERSION}/config/${NETWORK}/config.yml"
    
    if ! download_file "$config_url" "$INSTALL_DIR/config.yml"; then
        print_warning "Failed to download config, you may need to create it manually"
    else
        print_success "Configuration downloaded"
    fi
    
    # Create data directory
    mkdir -p "$INSTALL_DIR/fiber"
    
    print_success "Installation complete!"
    echo ""
    echo "FNN is installed at: $INSTALL_DIR/fnn"
    echo ""
    echo "Next steps:"
    echo "  1. Ensure ckb-cli is installed"
    echo "  2. Set up your CKB account and export the key"
    echo "  3. Set FIBER_SECRET_KEY_PASSWORD environment variable"
    echo "  4. Run: $INSTALL_DIR/fnn -c $INSTALL_DIR/config.yml -d $INSTALL_DIR"
    echo ""
    echo "For detailed setup instructions:"
    echo "  https://docs.fiber.world/"
}

# Show help
if [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
    echo "Fiber Network Node (FNN) Installer"
    echo ""
    echo "Usage:"
    echo "  curl -sSfL https://your-domain.com/install.sh | sh"
    echo ""
    echo "Environment Variables:"
    echo "  INSTALL_DIR       Installation directory (default: ~/.fiber)"
    echo "  FNN_VERSION       Version to install (default: 0.7.1)"
    echo "  NETWORK           Network to use: testnet|mainnet (default: testnet)"
    echo ""
    echo "Examples:"
    echo "  # Install to default location"
    echo "  curl -sSfL https://your-domain.com/install.sh | sh"
    echo ""
    echo "  # Install to custom location"
    echo "  curl -sSfL https://your-domain.com/install.sh | INSTALL_DIR=/opt/fiber sh"
    echo ""
    echo "  # Install specific version"
    echo "  curl -sSfL https://your-domain.com/install.sh | FNN_VERSION=0.6.1 sh"
    echo ""
    exit 0
fi

# Run installation
install_fn
