#!/bin/bash

if [ -n "${FIBER_INSTALL_COMMON_LOADED:-}" ]; then
    return 0 2>/dev/null || exit 0
fi
FIBER_INSTALL_COMMON_LOADED=1

INSTALL_REPO_DEFAULT="nervosnetwork/fiber"
INSTALL_REF_DEFAULT="main"
INSTALL_URL_DEFAULT=""
FNN_VERSION_DEFAULT="0.8.0"
CKB_CLI_VERSION_DEFAULT="1.12.0"
DEFAULT_MAINNET_CKB_RPC_URL="https://mainnet.ckb.dev/"
NETWORK_MARKER_FILE_NAME=".fiber-network"

setup_install_colors() {
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    NC='\033[0m'
}

print_header() {
    echo -e "${BLUE}"
    echo "=========================================="
    echo "    Fiber Network Node (FNN) Installer"
    echo "=========================================="
    echo -e "${NC}"
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

print_error() {
    echo -e "${RED}✗ $1${NC}"
}

print_info() {
    echo -e "${BLUE}ℹ $1${NC}"
}

check_command() {
    command -v "$1" &> /dev/null
}

init_install_defaults() {
    setup_install_colors
    INSTALL_REPO="${INSTALL_REPO:-$INSTALL_REPO_DEFAULT}"
    INSTALL_REF="${INSTALL_REF:-$INSTALL_REF_DEFAULT}"
    INSTALL_URL_DEFAULT="https://raw.githubusercontent.com/${INSTALL_REPO}/${INSTALL_REF}/tools/install/install-curl.sh"
    INSTALL_URL="${INSTALL_URL:-$INSTALL_URL_DEFAULT}"
    FNN_VERSION="${FNN_VERSION:-$FNN_VERSION_DEFAULT}"
    CKB_CLI_VERSION="${CKB_CLI_VERSION:-$CKB_CLI_VERSION_DEFAULT}"
    GITHUB_RELEASE_URL="https://github.com/nervosnetwork/fiber/releases/download/v${FNN_VERSION}"
    CKB_CLI_RELEASE_URL="https://github.com/nervosnetwork/ckb-cli/releases/download/v${CKB_CLI_VERSION}"
}

is_interactive_stdin() {
    [ -t 0 ]
}

validate_network() {
    local network="$1"

    if [ "$network" != "testnet" ] && [ "$network" != "mainnet" ]; then
        print_error "Invalid network: $network. Must be 'testnet' or 'mainnet'"
        exit 1
    fi
}

ensure_download_tool() {
    if ! check_command curl && ! check_command wget; then
        print_error "Neither curl nor wget is installed. Please install one of them."
        exit 1
    fi
}

download_file() {
    local url="$1"
    local output="$2"
    local mode="${3:-quiet}"

    if check_command curl; then
        if [ "$mode" = "progress" ]; then
            curl -fL -o "$output" "$url" --progress-bar
        else
            curl -fsSL "$url" -o "$output"
        fi
    elif check_command wget; then
        if [ "$mode" = "progress" ]; then
            wget --show-progress "$url" -O "$output"
        else
            wget -q --show-progress "$url" -O "$output"
        fi
    else
        print_error "Neither curl nor wget is installed"
        exit 1
    fi
}

extract_archive() {
    local archive_path="$1"
    local destination_dir="$2"

    case "$archive_path" in
        *.tar.gz)
            tar -xzf "$archive_path" -C "$destination_dir"
            ;;
        *.zip)
            unzip -q "$archive_path" -d "$destination_dir"
            ;;
        *)
            print_error "Unsupported archive format: $archive_path"
            exit 1
            ;;
    esac
}

find_first_file() {
    local search_dir="$1"
    local file_name="$2"

    find "$search_dir" -name "$file_name" -type f | head -1
}

find_first_dir() {
    local search_dir="$1"
    local dir_name="$2"

    find "$search_dir" -name "$dir_name" -type d | head -1
}

copy_required_binary_from_search_dir() {
    local search_dir="$1"
    local source_name="$2"
    local install_dir="$3"
    local target_name="$4"
    local description="$5"
    local binary_path

    binary_path=$(find_first_file "$search_dir" "$source_name")
    if [ -z "$binary_path" ]; then
        print_error "Could not find $source_name in the downloaded archive"
        exit 1
    fi

    copy_binary_to_install_dir "$binary_path" "$install_dir" "$target_name"
    print_success "$description installed to $install_dir/$target_name"
}

copy_required_directory_from_search_dir() {
    local search_dir="$1"
    local source_name="$2"
    local install_dir="$3"
    local target_name="$4"
    local description="$5"
    local dir_path

    dir_path=$(find_first_dir "$search_dir" "$source_name")
    if [ -z "$dir_path" ]; then
        print_error "Could not find $source_name directory in the downloaded archive"
        exit 1
    fi

    rm -rf "$install_dir/$target_name"
    cp -R "$dir_path" "$install_dir/$target_name"
    print_success "$description installed to $install_dir/$target_name"
}

copy_first_existing_file_to_install_dir() {
    local install_dir="$1"
    local target_name="$2"
    local description="$3"
    shift 3

    local candidate
    for candidate in "$@"; do
        if [ -f "$candidate" ]; then
            copy_binary_to_install_dir "$candidate" "$install_dir" "$target_name"
            print_success "$description installed to $install_dir/$target_name"
            return 0
        fi
    done

    return 1
}

copy_first_existing_directory_to_install_dir() {
    local install_dir="$1"
    local target_name="$2"
    local description="$3"
    shift 3

    local candidate
    for candidate in "$@"; do
        if [ -d "$candidate" ]; then
            rm -rf "$install_dir/$target_name"
            cp -R "$candidate" "$install_dir/$target_name"
            print_success "$description installed to $install_dir/$target_name"
            return 0
        fi
    done

    return 1
}

get_config_value_in_section() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"

    [ -f "$config_file" ] || return 0

    awk -v section_name="$section_name" -v key_name="$key_name" '
        /^[^[:space:]#][^:]*:[[:space:]]*$/ {
            in_section = ($0 == section_name ":")
            next
        }

        in_section && $0 ~ "^[[:space:]]*" key_name ":[[:space:]]*" {
            value = $0
            sub("^[[:space:]]*" key_name ":[[:space:]]*", "", value)
            sub(/[[:space:]]+#.*$/, "", value)
            sub(/^"/, "", value)
            sub(/"$/, "", value)
            print value
            exit
        }
    ' "$config_file"
}

set_config_value_in_section() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"
    local key_value="$4"
    local temp_file

    [ -f "$config_file" ] || return 1

    temp_file=$(mktemp)
    if ! awk -v section_name="$section_name" -v key_name="$key_name" -v key_value="$key_value" '
        /^[^[:space:]#][^:]*:[[:space:]]*$/ {
            in_section = ($0 == section_name ":")
            print
            next
        }

        in_section && $0 ~ "^[[:space:]]*" key_name ":[[:space:]]*" {
            printf "  %s: \"%s\"\n", key_name, key_value
            updated = 1
            next
        }

        {
            print
        }

        END {
            if (!updated) {
                exit 1
            }
        }
    ' "$config_file" > "$temp_file"; then
        rm -f "$temp_file"
        return 1
    fi

    mv "$temp_file" "$config_file"
}

get_existing_install_network() {
    local install_dir="$1"
    local marker_file="$install_dir/$NETWORK_MARKER_FILE_NAME"
    local config_file="$install_dir/config.yml"

    if [ -f "$marker_file" ]; then
        head -n 1 "$marker_file" | tr -d '\r'
        return 0
    fi

    get_config_value_in_section "$config_file" "fiber" "chain"
}

install_data_dir_has_contents() {
    local install_dir="$1"
    local data_dir="$install_dir/fiber"

    [ -d "$data_dir" ] || return 1

    find "$data_dir" -mindepth 1 -maxdepth 1 -print -quit 2> /dev/null | grep -q .
}

ensure_install_dir_matches_network() {
    local install_dir="$1"
    local network="$2"
    local existing_network

    existing_network="$(get_existing_install_network "$install_dir")"
    if [ -n "$existing_network" ] && [ "$existing_network" != "$network" ] && install_data_dir_has_contents "$install_dir"; then
        print_error "Install directory $install_dir already contains $existing_network data."
        echo "  Reusing it for $network can mix network graph state and cause chain hash mismatch warnings."
        echo "  Use a different install directory, or remove $install_dir/fiber before switching networks."
        exit 1
    fi
}

write_install_network_marker() {
    local install_dir="$1"
    local network="$2"

    printf '%s\n' "$network" > "$install_dir/$NETWORK_MARKER_FILE_NAME"
}

apply_network_config_defaults() {
    local install_dir="$1"
    local network="$2"
    local config_file="$install_dir/config.yml"
    local rpc_url_override="${CKB_RPC_URL:-}"

    if [ "$network" = "mainnet" ] && [ -z "$rpc_url_override" ]; then
        rpc_url_override="$DEFAULT_MAINNET_CKB_RPC_URL"
    fi

    if [ -n "$rpc_url_override" ]; then
        if ! set_config_value_in_section "$config_file" "ckb" "rpc_url" "$rpc_url_override"; then
            print_error "Failed to update ckb.rpc_url in $config_file"
            exit 1
        fi
    fi
}

copy_binary_to_install_dir() {
    local source_path="$1"
    local install_dir="$2"
    local target_name="$3"

    if [ ! -f "$source_path" ]; then
        print_error "Binary not found: $source_path"
        exit 1
    fi

    chmod +x "$source_path"
    cp "$source_path" "$install_dir/$target_name"
}

detect_platform() {
    PLATFORM=$(uname -s)
    ARCH=$(uname -m)
    FNN_BINARY_ARCH="$ARCH"

    case "$PLATFORM" in
        Linux*)
            case "$ARCH" in
                x86_64) FNN_BINARY_NAME="fnn_v${FNN_VERSION}-x86_64-linux-portable.tar.gz" ;;
                aarch64) FNN_BINARY_NAME="fnn_v${FNN_VERSION}-aarch64-linux-portable.tar.gz" ;;
                *) print_error "Unsupported architecture: $ARCH"; exit 1 ;;
            esac
            ;;
        Darwin*)
            case "$ARCH" in
                x86_64) FNN_BINARY_NAME="fnn_v${FNN_VERSION}-x86_64-darwin-portable.tar.gz" ;;
                arm64)
                    FNN_BINARY_ARCH="x86_64"
                    FNN_BINARY_NAME="fnn_v${FNN_VERSION}-x86_64-darwin-portable.tar.gz"
                    ;;
                *) print_error "Unsupported architecture: $ARCH"; exit 1 ;;
            esac
            ;;
        *)
            print_error "Unsupported platform: $PLATFORM"
            exit 1
            ;;
    esac
}

resolve_ckb_cli_binary_name() {
    case "$PLATFORM" in
        Linux*)
            case "$ARCH" in
                x86_64) echo "ckb-cli_v${CKB_CLI_VERSION}_x86_64-unknown-linux-gnu.tar.gz" ;;
                aarch64) echo "ckb-cli_v${CKB_CLI_VERSION}_aarch64-unknown-linux-gnu.tar.gz" ;;
                *) print_error "Unsupported architecture for ckb-cli: $ARCH"; exit 1 ;;
            esac
            ;;
        Darwin*)
            case "$ARCH" in
                x86_64) echo "ckb-cli_v${CKB_CLI_VERSION}_x86_64-apple-darwin.zip" ;;
                arm64) echo "ckb-cli_v${CKB_CLI_VERSION}_aarch64-apple-darwin.zip" ;;
                *) print_error "Unsupported architecture for ckb-cli: $ARCH"; exit 1 ;;
            esac
            ;;
        *)
            print_error "Unsupported platform for ckb-cli: $PLATFORM"
            exit 1
            ;;
    esac
}

require_unzip_if_needed() {
    if [ "$PLATFORM" = "Darwin" ] && ! check_command unzip; then
        print_error "unzip is required on macOS. Please install it:"
        echo "  brew install unzip"
        exit 1
    fi
}

install_fnn_binary() {
    local install_dir="$1"
    local temp_dir
    local extract_dir
    local download_url="${GITHUB_RELEASE_URL}/${FNN_BINARY_NAME}"
    temp_dir=$(mktemp -d)
    extract_dir="$temp_dir/extracted"
    mkdir -p "$extract_dir"

    print_info "Downloading Fiber release bundle v${FNN_VERSION} for ${PLATFORM}-${FNN_BINARY_ARCH}..."
    echo "  Downloading from: $download_url"

    if ! download_file "$download_url" "${temp_dir}/${FNN_BINARY_NAME}" progress; then
        print_error "Failed to download FNN binary"
        rm -rf "$temp_dir"
        exit 1
    fi

    print_success "Download completed"
    print_info "Extracting release bundle..."
    extract_archive "${temp_dir}/${FNN_BINARY_NAME}" "$extract_dir"

    copy_required_binary_from_search_dir "$extract_dir" "fnn" "$install_dir" "fnn" "Node binary"
    copy_required_binary_from_search_dir "$extract_dir" "fnn-cli" "$install_dir" "fnn-cli" "CLI binary"
    copy_required_binary_from_search_dir "$extract_dir" "fnn-migrate" "$install_dir" "fnn-migrate" "Migration binary"
    copy_required_directory_from_search_dir "$extract_dir" "config" "$install_dir" "config" "Configuration templates"
    rm -rf "$temp_dir"
}

install_local_fnn_binary() {
    local source_path="$1"
    local install_dir="$2"
    local source_abs
    local source_dir
    local migrate_candidate

    if [ ! -f "$source_path" ]; then
        print_error "Local fnn binary not found: $source_path"
        exit 1
    fi

    source_abs="$(cd "$(dirname "$source_path")" && pwd)/$(basename "$source_path")"
    source_dir="$(dirname "$source_abs")"
    print_info "Using local FNN binary: $source_abs"
    copy_binary_to_install_dir "$source_abs" "$install_dir" "fnn"
    print_success "Node binary installed to $install_dir/fnn"

    if ! copy_first_existing_file_to_install_dir \
        "$install_dir" \
        "fnn-cli" \
        "CLI binary" \
        "$source_dir/fnn-cli"
    then
        print_warning "Local fnn-cli binary not found next to $source_abs."
    fi

    for migrate_candidate in \
        "$source_dir/fnn-migrate" \
        "$source_dir/../../migrate/target/release/fnn-migrate"
    do
        if [ -f "$migrate_candidate" ]; then
            copy_binary_to_install_dir "$migrate_candidate" "$install_dir" "fnn-migrate"
            print_success "Migration binary installed to $install_dir/fnn-migrate"
            break
        fi
    done

    if [ ! -f "$install_dir/fnn-migrate" ]; then
        print_warning "Local fnn-migrate binary not found next to $source_abs."
        echo "  Copy fnn-migrate into $install_dir manually if you need to run database migrations."
    fi

    if ! copy_first_existing_directory_to_install_dir \
        "$install_dir" \
        "config" \
        "Configuration templates" \
        "$source_dir/config" \
        "$source_dir/../../config"
    then
        print_warning "Local config directory not found near $source_abs."
    fi
}

download_config_file() {
    local install_dir="$1"
    local network="$2"
    local bundled_config="$install_dir/config/${network}/config.yml"
    local template_network
    local missing_templates=0

    print_info "Preparing configuration files..."
    mkdir -p "$install_dir/config"

    for template_network in testnet mainnet; do
        if [ ! -f "$install_dir/config/${template_network}/config.yml" ]; then
            missing_templates=1
            mkdir -p "$install_dir/config/${template_network}"
            if ! download_file \
                "https://raw.githubusercontent.com/nervosnetwork/fiber/v${FNN_VERSION}/config/${template_network}/config.yml" \
                "$install_dir/config/${template_network}/config.yml" \
                quiet
            then
                print_error "Failed to download config for ${template_network}"
                exit 1
            fi
        fi
    done

    if [ ! -f "$bundled_config" ]; then
        print_error "Could not prepare the selected config template: $bundled_config"
        exit 1
    fi

    cp "$bundled_config" "$install_dir/config.yml"
    apply_network_config_defaults "$install_dir" "$network"
    write_install_network_marker "$install_dir" "$network"

    if [ "$missing_templates" -eq 1 ]; then
        print_success "Configuration prepared with downloaded network templates"
    else
        print_success "Configuration prepared from bundled network templates"
    fi
}

install_ckb_cli_binary() {
    local install_dir="$1"
    local ckb_cli_binary
    local download_url
    local temp_dir
    local user_bin_dir="$HOME/.local/bin"
    local ckb_cli_path

    ckb_cli_binary="$(resolve_ckb_cli_binary_name)"
    download_url="${CKB_CLI_RELEASE_URL}/${ckb_cli_binary}"
    temp_dir=$(mktemp -d)

    print_info "Downloading ckb-cli v${CKB_CLI_VERSION}..."
    echo "  Downloading from: $download_url"

    download_file "$download_url" "${temp_dir}/${ckb_cli_binary}" progress

    print_info "Extracting ckb-cli..."
    extract_archive "${temp_dir}/${ckb_cli_binary}" "$temp_dir"

    ckb_cli_path=$(find_first_file "$temp_dir" "ckb-cli")
    if [ -z "$ckb_cli_path" ]; then
        print_error "Could not find ckb-cli binary in the downloaded archive"
        rm -rf "$temp_dir"
        exit 1
    fi

    chmod +x "$ckb_cli_path"

    if [ -w "/usr/local/bin" ]; then
        cp "$ckb_cli_path" /usr/local/bin/
        CKB_CLI_INSTALLED_PATH="/usr/local/bin/ckb-cli"
        print_success "ckb-cli installed to /usr/local/bin/"
    elif mkdir -p "$user_bin_dir" 2> /dev/null; then
        cp "$ckb_cli_path" "$user_bin_dir/"
        CKB_CLI_INSTALLED_PATH="$user_bin_dir/ckb-cli"
        print_success "ckb-cli installed to $user_bin_dir/"
        print_warning "Please ensure $user_bin_dir is in your PATH"
    else
        copy_binary_to_install_dir "$ckb_cli_path" "$install_dir" "ckb-cli"
        CKB_CLI_INSTALLED_PATH="$install_dir/ckb-cli"
        print_success "ckb-cli installed to $install_dir/ckb-cli"
        print_warning "To use ckb-cli from command line, add $install_dir to your PATH:"
        echo "  export PATH=\"$install_dir:\$PATH\""
    fi

    rm -rf "$temp_dir"
}
