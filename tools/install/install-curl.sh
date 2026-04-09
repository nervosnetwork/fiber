#!/bin/bash
# Fiber Network Node (FNN) Install Script
# Usage: curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | bash
# Or: curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | bash -s -- --help

set -e

source_install_common() {
    local script_dir=""
    local install_repo="${INSTALL_REPO:-nervosnetwork/fiber}"
    local install_ref="${INSTALL_REF:-main}"

    if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
        script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    fi

    if [ -n "$script_dir" ] && [ -f "$script_dir/install-common.sh" ]; then
        # shellcheck source=install-common.sh
        . "$script_dir/install-common.sh"
        return
    fi

    local install_url="${INSTALL_URL:-https://raw.githubusercontent.com/${install_repo}/${install_ref}/tools/install/install-curl.sh}"
    local common_url="${INSTALL_COMMON_URL:-}"
    local temp_common
    temp_common=$(mktemp)

    if [ -z "$common_url" ]; then
        case "$install_url" in
            */install-curl.sh|*/install.sh)
                common_url="${install_url%/*}/install-common.sh"
                ;;
            *)
                common_url="${install_url%/}/install-common.sh"
                ;;
        esac
    fi

    if command -v curl &> /dev/null; then
        curl -fsSL "$common_url" -o "$temp_common"
    elif command -v wget &> /dev/null; then
        wget -q "$common_url" -O "$temp_common"
    else
        echo "Error: Neither curl nor wget is installed" >&2
        rm -f "$temp_common"
        exit 1
    fi

    # shellcheck source=/dev/null
    . "$temp_common"
    rm -f "$temp_common"
}

source_install_common
init_install_defaults

# Configuration
INSTALL_DIR="${INSTALL_DIR:-$HOME/.fiber}"
NETWORK="${NETWORK:-testnet}"
detect_platform
CKB_CLI_HINT_PATH=""

get_local_install_script_dir() {
    if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
        cd "$(dirname "${BASH_SOURCE[0]}")" && pwd
    fi
}

resolve_install_script_url() {
    local script_name="$1"
    local override_url="${2:-}"
    local install_repo="${INSTALL_REPO:-nervosnetwork/fiber}"
    local install_ref="${INSTALL_REF:-main}"
    local install_url="${INSTALL_URL:-https://raw.githubusercontent.com/${install_repo}/${install_ref}/tools/install/install-curl.sh}"

    if [ -n "$override_url" ]; then
        printf '%s\n' "$override_url"
        return
    fi

    case "$install_url" in
        */install-curl.sh|*/install.sh)
            printf '%s/%s\n' "${install_url%/*}" "$script_name"
            ;;
        *)
            printf '%s/%s\n' "${install_url%/}" "$script_name"
            ;;
    esac
}

download_unix_installer_scripts() {
    local helper_dir="$INSTALL_DIR/tools/install"
    local local_script_dir
    local quick_start_url
    local common_url

    mkdir -p "$helper_dir"
    local_script_dir="$(get_local_install_script_dir)"

    if [ -n "$local_script_dir" ] && [ -f "$local_script_dir/quick-start.sh" ] && [ -f "$local_script_dir/install-common.sh" ]; then
        cp "$local_script_dir/quick-start.sh" "$helper_dir/quick-start.sh"
        cp "$local_script_dir/install-common.sh" "$helper_dir/install-common.sh"
    else
        quick_start_url="$(resolve_install_script_url "quick-start.sh" "${INSTALL_QUICK_START_URL:-}")"
        common_url="$(resolve_install_script_url "install-common.sh" "${INSTALL_COMMON_URL:-}")"

        print_info "Downloading Unix installer scripts..."
        download_file "$quick_start_url" "$helper_dir/quick-start.sh" quiet
        download_file "$common_url" "$helper_dir/install-common.sh" quiet
    fi

    chmod +x "$helper_dir/quick-start.sh" "$helper_dir/install-common.sh"
    print_success "Unix installer scripts installed to $helper_dir"
}

ensure_ckb_cli_available() {
    if CKB_CLI_HINT_PATH="$(resolve_existing_ckb_cli_path "$INSTALL_DIR")"; then
        print_success "ckb-cli found at $CKB_CLI_HINT_PATH"
        return
    fi

    require_unzip_if_needed
    print_warning "ckb-cli not found. Downloading it automatically..."
    install_ckb_cli_binary "$INSTALL_DIR"
    CKB_CLI_HINT_PATH="$CKB_CLI_INSTALLED_PATH"
}

install_fn() {
    print_header

    # Non-interactive mode uses defaults
    if ! is_interactive_stdin; then
        print_info "Running in non-interactive mode with defaults"
        print_info "Install directory: $INSTALL_DIR"
        print_info "Network: $NETWORK"
        print_info "Version: $FNN_VERSION"
        echo ""
    fi

    validate_network "$NETWORK"
    ensure_install_dir_matches_network "$INSTALL_DIR" "$NETWORK"
    ensure_download_tool

    # Create install directory
    mkdir -p "$INSTALL_DIR"

    ensure_ckb_cli_available

    install_fnn_binary "$INSTALL_DIR"
    download_config_file "$INSTALL_DIR" "$NETWORK"
    download_unix_installer_scripts

    # Create data directory
    mkdir -p "$INSTALL_DIR/fiber"

    cd "$INSTALL_DIR"

    print_success "Installation complete!"
    echo ""
    echo "Release bundle installed to: $INSTALL_DIR"
    echo "  - $INSTALL_DIR/fnn"
    echo "  - $INSTALL_DIR/fnn-cli"
    echo "  - $INSTALL_DIR/fnn-migrate"
    echo "  - $INSTALL_DIR/config"
    echo "  - $INSTALL_DIR/tools/install/quick-start.sh"
    echo "ckb-cli is available at: $CKB_CLI_HINT_PATH"
    echo ""
    echo "Next steps:"
    echo ""
    echo "For detailed setup instructions:"
    echo "  https://www.fiber.world/docs/quick-start/run-a-node"
    echo ""
    echo "  If you want the guided Unix installer later:"
    echo "     $INSTALL_DIR/tools/install/quick-start.sh $INSTALL_DIR"

}

# Show help
if [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
    echo "Fiber Network Node (FNN) Installer"
    echo ""
    echo "Usage:"
    echo "  curl -sSfL $INSTALL_URL | bash"
    echo ""
    echo "Environment Variables:"
    echo "  INSTALL_DIR       Installation directory (default: ~/.fiber)"
    echo "  INSTALL_REPO      GitHub repo for raw installer files (default: nervosnetwork/fiber)"
    echo "  INSTALL_REF       Git ref for raw installer files (default: main)"
    echo "  INSTALL_QUICK_START_URL"
    echo "                    Override URL for quick-start.sh"
    echo "  INSTALL_COMMON_URL"
    echo "                    Override URL for install-common.sh"
    echo "  FNN_VERSION       Version to install (default: 0.8.0)"
    echo "  NETWORK           Network to use: testnet|mainnet (default: testnet)"
    echo ""
    echo "Behavior:"
    echo "  The installer extracts the full release bundle: fnn, fnn-cli, fnn-migrate, and config/."
    echo "  If ckb-cli is not installed, the installer downloads it automatically."
    echo "  It also installs quick-start.sh and install-common.sh under INSTALL_DIR/tools/install/."
    echo ""
    echo "Examples:"
    echo "  # Install to default location"
    echo "  curl -sSfL $INSTALL_URL | bash"
    echo ""
    echo "  # Install to custom location"
    echo "  curl -sSfL $INSTALL_URL | INSTALL_DIR=/opt/fiber bash"
    echo ""
    echo "  # Install specific version"
    echo "  curl -sSfL $INSTALL_URL | FNN_VERSION=0.8.0 bash"
    echo ""
    exit 0
fi

# Run installation
install_fn
