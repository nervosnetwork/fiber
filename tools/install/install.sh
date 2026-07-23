#!/bin/bash

# Fiber Network Node (FNN) Installer Script
# Usage:
#   ./tools/install/install.sh [install-directory] [network]
#   ./tools/install/install.sh --local-binary /path/to/fnn [install-directory] [network]
#   curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install.sh | bash

INSTALL_REPO_DEFAULT="nervosnetwork/fiber"
INSTALL_REF_DEFAULT="main"
INSTALL_URL_DEFAULT=""
FNN_VERSION_DEFAULT="0.8.0"
CKB_CLI_VERSION_DEFAULT="1.12.0"
NETWORK_MARKER_FILE_NAME=".fiber-network"

setup_install_colors() {
    RED="$(printf '\033[0;31m')"
    GREEN="$(printf '\033[0;32m')"
    YELLOW="$(printf '\033[1;33m')"
    BLUE="$(printf '\033[0;34m')"
    NC="$(printf '\033[0m')"
}

print_header() {
    printf '%s\n' \
        "${BLUE}==========================================" \
        "    Fiber Network Node (FNN) Installer" \
        "==========================================${NC}"
}

print_success() {
    printf '%s\n' "${GREEN}✓ $1${NC}"
}

print_warning() {
    printf '%s\n' "${YELLOW}⚠ $1${NC}"
}

print_error() {
    printf '%s\n' "${RED}✗ $1${NC}"
}

print_info() {
    printf '%s\n' "${BLUE}ℹ $1${NC}"
}

ensure_success() {
    local error_message="$1"
    shift

    "$@" || {
        print_error "$error_message"
        exit 1
    }
}

update_config_value_or_exit() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"
    local key_value="$4"

    ensure_success \
        "Failed to update ${section_name}.${key_name} in $config_file" \
        set_config_value_in_section "$config_file" "$section_name" "$key_name" "$key_value"
}

update_rendered_config_value_or_exit() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"
    local rendered_value="$4"

    ensure_success \
        "Failed to update ${section_name}.${key_name} in $config_file" \
        set_rendered_config_value_in_section "$config_file" "$section_name" "$key_name" "$rendered_value"
}

remove_config_value_or_exit() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"

    ensure_success \
        "Failed to update ${section_name}.${key_name} in $config_file" \
        remove_config_value_in_section "$config_file" "$section_name" "$key_name"
}

update_list_config_value_or_exit() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"
    shift 3

    ensure_success \
        "Failed to update ${section_name}.${key_name} in $config_file" \
        set_list_value_in_section "$config_file" "$section_name" "$key_name" "$@"
}

check_command() {
    command -v "$1" &> /dev/null
}

init_install_defaults() {
    setup_install_colors
    INSTALL_REPO="${INSTALL_REPO:-$INSTALL_REPO_DEFAULT}"
    INSTALL_REF="${INSTALL_REF:-$INSTALL_REF_DEFAULT}"
    INSTALL_URL_DEFAULT="https://raw.githubusercontent.com/${INSTALL_REPO}/${INSTALL_REF}/tools/install/install.sh"
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

find_first_path() {
    local search_dir="$1"
    local artifact_name="$2"
    local artifact_type="$3"

    find "$search_dir" -name "$artifact_name" -type "$artifact_type" | head -1
}

install_artifact_into_dir() {
    local source_path="$1"
    local install_dir="$2"
    local target_name="$3"
    local description="$4"
    local artifact_kind="$5"

    if [ "$artifact_kind" = "file" ]; then
        copy_binary_to_install_dir "$source_path" "$install_dir" "$target_name"
    else
        rm -rf "$install_dir/$target_name"
        cp -R "$source_path" "$install_dir/$target_name"
    fi

    print_success "$description installed to $install_dir/$target_name"
}

install_required_artifact_from_search_dir() {
    local search_dir="$1"
    local source_name="$2"
    local install_dir="$3"
    local target_name="$4"
    local description="$5"
    local artifact_kind="$6"
    local artifact_path=""
    local artifact_type="f"

    if [ "$artifact_kind" = "dir" ]; then
        artifact_type="d"
    fi

    artifact_path=$(find_first_path "$search_dir" "$source_name" "$artifact_type")
    if [ -z "$artifact_path" ]; then
        if [ "$artifact_kind" = "dir" ]; then
            print_error "Could not find $source_name directory in the downloaded archive"
        else
            print_error "Could not find $source_name in the downloaded archive"
        fi
        exit 1
    fi

    install_artifact_into_dir "$artifact_path" "$install_dir" "$target_name" "$description" "$artifact_kind"
}

install_first_existing_artifact() {
    local install_dir="$1"
    local target_name="$2"
    local description="$3"
    local artifact_kind="$4"
    shift 4

    local candidate
    for candidate in "$@"; do
        if [ "$artifact_kind" = "dir" ] && [ -d "$candidate" ]; then
            install_artifact_into_dir "$candidate" "$install_dir" "$target_name" "$description" "$artifact_kind"
            return 0
        fi
        if [ "$artifact_kind" = "file" ] && [ -f "$candidate" ]; then
            install_artifact_into_dir "$candidate" "$install_dir" "$target_name" "$description" "$artifact_kind"
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

config_key_exists_in_section() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"

    [ -f "$config_file" ] || return 1

    awk -v section_name="$section_name" -v key_name="$key_name" '
        /^[^[:space:]#][^:]*:[[:space:]]*$/ {
            in_section = ($0 == section_name ":")
            next
        }

        in_section && $0 ~ "^[[:space:]]*" key_name ":[[:space:]]*" {
            found = 1
            exit
        }

        END {
            if (!found) {
                exit 1
            }
        }
    ' "$config_file" > /dev/null
}

rewrite_config_file() {
    local config_file="$1"
    shift
    local temp_file

    [ -f "$config_file" ] || return 1

    temp_file=$(mktemp)
    if ! awk "$@" "$config_file" > "$temp_file"; then
        rm -f "$temp_file"
        return 1
    fi

    mv "$temp_file" "$config_file"
}

set_config_value_in_section() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"
    local key_value="$4"
    local escaped_key_value

    config_key_exists_in_section "$config_file" "$section_name" "$key_name" || return 1
    escaped_key_value="$(escape_yaml_double_quoted_value "$key_value")"
    set_rendered_config_value_in_section "$config_file" "$section_name" "$key_name" "\"$escaped_key_value\""
}

set_rendered_config_value_in_section() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"
    local rendered_value="$4"

    rewrite_config_file "$config_file" \
        -v section_name="$section_name" \
        -v key_name="$key_name" \
        -v rendered_value="$rendered_value" '
        /^[^[:space:]#][^:]*:[[:space:]]*$/ {
            if (in_section && !updated) {
                printf "  %s: %s\n", key_name, rendered_value
                updated = 1
            }
            in_section = ($0 == section_name ":")
            print
            next
        }

        in_section && $0 ~ "^[[:space:]]*" key_name ":[[:space:]]*" {
            printf "  %s: %s\n", key_name, rendered_value
            updated = 1
            next
        }

        {
            print
        }

        END {
            if (in_section && !updated) {
                printf "  %s: %s\n", key_name, rendered_value
            }
        }
    '
}

remove_config_value_in_section() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"

    rewrite_config_file "$config_file" \
        -v section_name="$section_name" \
        -v key_name="$key_name" '
        /^[^[:space:]#][^:]*:[[:space:]]*$/ {
            in_section = ($0 == section_name ":")
            print
            next
        }

        in_section && $0 ~ "^[[:space:]]*" key_name ":[[:space:]]*" {
            next
        }

        {
            print
        }
    '
}

set_list_value_in_section() {
    local config_file="$1"
    local section_name="$2"
    local key_name="$3"
    shift 3

    local list_delimiter
    local list_items=""
    local item

    list_delimiter=$'\034'
    for item in "$@"; do
        if [ -n "$list_items" ]; then
            list_items="${list_items}${list_delimiter}"
        fi
        list_items="${list_items}${item}"
    done

    rewrite_config_file "$config_file" \
        -v section_name="$section_name" \
        -v key_name="$key_name" \
        -v list_items="$list_items" \
        -v list_delimiter="$list_delimiter" '
        BEGIN {
            item_count = split(list_items, items, list_delimiter)
            if (item_count > 0 && items[item_count] == "") {
                item_count--
            }
        }

        function print_list_block() {
            if (item_count == 0) {
                printf "  %s: []\n", key_name
                return
            }

            printf "  %s:\n", key_name
            for (i = 1; i <= item_count; i++) {
                printf "    - \"%s\"\n", items[i]
            }
        }

        /^[^[:space:]#][^:]*:[[:space:]]*$/ {
            in_section = ($0 == section_name ":")
            print
            next
        }

        in_section && $0 ~ "^[[:space:]]*" key_name ":[[:space:]]*" {
            print_list_block()
            found = 1
            replacing = 1
            next
        }

        replacing {
            if ($0 ~ /^  [^[:space:]#][^:]*:[[:space:]]*/) {
                replacing = 0
                print
            }
            next
        }

        {
            print
        }

        END {
            if (!found) {
                exit 1
            }
        }
    '
}

escape_yaml_double_quoted_value() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
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

resolve_existing_ckb_cli_path() {
    local install_dir="$1"

    if check_command ckb-cli; then
        command -v ckb-cli
        return 0
    fi

    if [ -f "$install_dir/ckb-cli" ]; then
        printf '%s\n' "$install_dir/ckb-cli"
        return 0
    fi

    return 1
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
    local config_file="$install_dir/config.yml"
    local rpc_url_override="${CKB_RPC_URL:-}"

    if [ -n "$rpc_url_override" ]; then
        update_config_value_or_exit "$config_file" "ckb" "rpc_url" "$rpc_url_override"
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
                    FNN_BINARY_ARCH="aarch64"
                    FNN_BINARY_NAME="fnn_v${FNN_VERSION}-aarch64-darwin-portable.tar.gz"
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
        if [ "$PLATFORM" = "Darwin" ] && [ "$ARCH" = "arm64" ]; then
            echo "  This installer no longer falls back to the x86_64 Darwin bundle on Apple Silicon."
            echo "  Publish ${FNN_BINARY_NAME}, choose a release version that includes it, or use --local-binary."
        fi
        rm -rf "$temp_dir"
        exit 1
    fi

    print_success "Download completed"
    print_info "Extracting release bundle..."
    extract_archive "${temp_dir}/${FNN_BINARY_NAME}" "$extract_dir"

    install_required_artifact_from_search_dir "$extract_dir" "fnn" "$install_dir" "fnn" "Node binary" file
    install_required_artifact_from_search_dir "$extract_dir" "fnn-cli" "$install_dir" "fnn-cli" "CLI binary" file
    install_required_artifact_from_search_dir "$extract_dir" "fnn-migrate" "$install_dir" "fnn-migrate" "Migration binary" file
    install_required_artifact_from_search_dir "$extract_dir" "config" "$install_dir" "config" "Configuration templates" dir
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

    if ! install_first_existing_artifact \
        "$install_dir" \
        "fnn-cli" \
        "CLI binary" \
        file \
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

    if ! install_first_existing_artifact \
        "$install_dir" \
        "config" \
        "Configuration templates" \
        dir \
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
    apply_network_config_defaults "$install_dir"
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

    ckb_cli_path=$(find_first_path "$temp_dir" "ckb-cli" f)
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

# Global variable for ckb-cli command
CKB_CLI_CMD="ckb-cli"
CKB_CLI_HINT_PATH=""
LOCAL_FNN_BINARY=""
MAINNET_GENESIS_HASH="0x92b197aa1fba0f63633922c61c92375c9c074a93e85963554f5499fe1450d0e5"
TESTNET_GENESIS_HASH="0x10639e0895502b5688a6be8cf69460d76541bfa4821629d86d62ba0aae3f9606"
STARTUP_BLOCKER_MESSAGE=""
REUSE_EXISTING_INSTALL=0
PUBLIC_NODE_ANNOUNCED_ADDR_PLACEHOLDER="/ip4/YOUR-FIBER-NODE-PUBLIC-IP/tcp/8228"
PUBLIC_NODE_NAME_PLACEHOLDER="my-fiber-node"
INSTALL_MODE=""
POSITIONAL_ARGS=()

init_install_defaults

show_help() {
    cat <<EOF
Fiber Network Node (FNN) installer

Usage:
  ./tools/install/install.sh [install-directory] [network]
  ./tools/install/install.sh --local-binary /path/to/fnn [install-directory] [network]
  ./tools/install/install.sh --mode bootstrap [install-directory] [network]
  curl -sSfL ${INSTALL_URL:-https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install.sh} | bash

Options:
  --mode MODE           Select installer mode: guided or bootstrap.
  --local-binary PATH   Use an existing local fnn binary instead of downloading the release bundle.
  -h, --help            Show this help message.

Behavior:
  - Auto mode selects guided mode when stdin is interactive, otherwise bootstrap mode.
  - Guided mode reuses an existing installed bundle by default and backs up non-empty install directories.
  - Guided mode defaults to mainnet when no network is provided.
  - Bootstrap mode matches the one-liner installer behavior and defaults to ~/.fiber on testnet.
  - Non-interactive mainnet installs require CKB_RPC_URL to be set explicitly.
  - Mainnet guided installs ask whether this should be a public Fiber node.
EOF
}

get_script_install_root() {
    local script_dir
    local installed_root

    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    installed_root="$(cd "$script_dir/../.." && pwd)"

    if [ -f "$installed_root/fnn" ] && [ -d "$installed_root/tools/install" ]; then
        printf '%s\n' "$installed_root"
    fi
}

has_release_bundle() {
    local install_dir="$1"

    [ -f "$install_dir/fnn" ] &&
        [ -f "$install_dir/fnn-cli" ] &&
        [ -f "$install_dir/fnn-migrate" ] &&
        [ -d "$install_dir/config" ]
}

resolve_guided_install_dir() {
    local installed_root

    if [ -n "${INSTALL_DIR:-}" ]; then
        printf '%s\n' "$INSTALL_DIR"
        return
    fi

    installed_root="$(get_script_install_root)"
    if [ -n "$installed_root" ]; then
        printf '%s\n' "$installed_root"
        return
    fi

    printf '%s\n' "./my-fnn"
}

normalize_install_dir() {
    local install_dir="$1"

    case "$install_dir" in
        "~")
            printf '%s\n' "$HOME"
            ;;
        "~/"*)
            printf '%s/%s\n' "$HOME" "${install_dir#~/}"
            ;;
        *)
            printf '%s\n' "$install_dir"
            ;;
    esac
}

install_path_has_existing_contents() {
    local install_dir="$1"

    if [ ! -e "$install_dir" ]; then
        return 1
    fi

    if [ ! -d "$install_dir" ]; then
        return 0
    fi

    find "$install_dir" -mindepth 1 -maxdepth 1 -print -quit 2> /dev/null | grep -q .
}

generate_install_backup_dir() {
    local install_dir="$1"
    local timestamp
    local candidate
    local suffix=1

    timestamp="$(date +%Y%m%d-%H%M%S)"
    candidate="${install_dir}.backup-${timestamp}"

    while [ -e "$candidate" ]; do
        candidate="${install_dir}.backup-${timestamp}-${suffix}"
        suffix=$((suffix + 1))
    done

    printf '%s\n' "$candidate"
}

backup_existing_install_path() {
    local install_dir="$1"
    local backup_dir="$2"
    local backed_up_ckb_cli="$backup_dir/ckb-cli"
    local restored_ckb_cli="$install_dir/ckb-cli"

    mv "$install_dir" "$backup_dir"
    print_success "Backed up existing install path to $backup_dir"

    if [ -f "$backed_up_ckb_cli" ]; then
        mkdir -p "$install_dir"
        cp "$backed_up_ckb_cli" "$restored_ckb_cli"
        chmod +x "$restored_ckb_cli"
        print_success "Preserved existing ckb-cli at $restored_ckb_cli"
    fi
}

prepare_install_dir() {
    local backup_dir
    local script_install_root
    local user_input

    INSTALL_DIR="$(normalize_install_dir "$INSTALL_DIR")"
    script_install_root="$(get_script_install_root)"

    if [ -n "$script_install_root" ] && [ "$INSTALL_DIR" = "$script_install_root" ] && has_release_bundle "$INSTALL_DIR"; then
        REUSE_EXISTING_INSTALL=1
        print_info "Reusing the existing release bundle in $INSTALL_DIR"
        return
    fi

    while install_path_has_existing_contents "$INSTALL_DIR"; do
        backup_dir="$(generate_install_backup_dir "$INSTALL_DIR")"
        print_warning "Install directory already exists and is not empty: $INSTALL_DIR"

        if ! is_interactive_stdin; then
            backup_existing_install_path "$INSTALL_DIR" "$backup_dir"
            break
        fi

        echo "  Press Enter to back it up to:"
        echo "    $backup_dir"
        echo "  Or type a different install directory path."
        read -p "Install directory choice (default: back up current directory): " user_input

        if [ -z "$user_input" ]; then
            backup_existing_install_path "$INSTALL_DIR" "$backup_dir"
            break
        fi

        INSTALL_DIR="$(normalize_install_dir "$user_input")"
        print_info "Using installation directory: $INSTALL_DIR"
    done
}

prompt_for_network_if_needed() {
    if [ -n "$NETWORK" ]; then
        return
    fi

    if [ "$INSTALL_MODE" = "bootstrap" ]; then
        NETWORK="testnet"
        return
    fi

    if ! is_interactive_stdin; then
        NETWORK="mainnet"
        return
    fi

    echo "Choose network:"
    echo "  1) mainnet (default)"
    echo "  2) testnet"
    read -p "Enter your choice (1 or 2, default: mainnet): " network_choice
    network_choice=${network_choice:-1}

    case "$network_choice" in
        1)
            NETWORK="mainnet"
            ;;
        2)
            NETWORK="testnet"
            ;;
        *)
            print_error "Invalid choice: $network_choice"
            exit 1
            ;;
    esac
}

get_ckb_rpc_url_from_config() {
    get_config_value_in_section "$INSTALL_DIR/config.yml" "ckb" "rpc_url"
}

configure_ckb_rpc_url() {
    local desired_rpc_url=""

    if [ -n "${CKB_RPC_URL:-}" ]; then
        update_config_value_or_exit "$INSTALL_DIR/config.yml" "ckb" "rpc_url" "$CKB_RPC_URL"
        print_success "Configured CKB RPC URL: $CKB_RPC_URL"
        return
    fi

    if [ "$NETWORK" != "mainnet" ]; then
        return
    fi

    if ! is_interactive_stdin; then
        print_error "Mainnet installs require an explicitly configured CKB RPC endpoint."
        echo "  Set CKB_RPC_URL to a trusted mainnet CKB RPC URL and run the installer again."
        exit 1
    fi

    echo ""
    print_warning "Mainnet requires a reachable CKB RPC endpoint."
    echo "  No public RPC endpoint is selected automatically."
    while [ -z "$desired_rpc_url" ]; do
        if ! read -r -p "Enter a trusted mainnet CKB RPC URL: " desired_rpc_url; then
            print_error "Could not read the CKB RPC URL."
            exit 1
        fi
        desired_rpc_url="$(printf '%s' "$desired_rpc_url" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
        if [ -z "$desired_rpc_url" ]; then
            print_warning "The CKB RPC URL cannot be empty."
        fi
    done

    update_config_value_or_exit "$INSTALL_DIR/config.yml" "ckb" "rpc_url" "$desired_rpc_url"
    print_success "Configured CKB RPC URL: $desired_rpc_url"
}

configure_mainnet_public_node() {
    local config_file="$INSTALL_DIR/config.yml"
    local public_choice
    local announced_addr=""
    local announced_node_name=""
    local escaped_node_name
    local is_public=0

    if [ "$NETWORK" != "mainnet" ] || ! is_interactive_stdin; then
        return
    fi

    echo ""
    read -p "Should this mainnet node be a public Fiber node? (y/n, default: n): " public_choice
    public_choice="${public_choice:-n}"

    case "$public_choice" in
        y|Y|yes|YES)
            is_public=1
            ;;
        n|N|no|NO)
            is_public=0
            ;;
        *)
            print_error "Invalid choice: $public_choice"
            exit 1
            ;;
    esac

    if [ "$is_public" -eq 1 ]; then
        echo ""
        print_info "Configure the public address announced to the Fiber network."
        echo "  Placeholder: $PUBLIC_NODE_ANNOUNCED_ADDR_PLACEHOLDER"
        while [ -z "$announced_addr" ]; do
            read -p "Enter announced_addrs: " announced_addr
            if [ -z "$announced_addr" ]; then
                print_warning "announced_addrs cannot be empty for a public mainnet node."
            fi
        done

        echo ""
        print_info "Configure the node name announced to the Fiber network."
        echo "  Placeholder: $PUBLIC_NODE_NAME_PLACEHOLDER"
        echo "  Press Enter to skip announced_node_name."
        read -p "Enter announced_node_name (optional): " announced_node_name
        announced_node_name="$(printf '%s' "$announced_node_name" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"

        update_rendered_config_value_or_exit "$config_file" "fiber" "auto_announce_node" "true"
        update_rendered_config_value_or_exit "$config_file" "fiber" "announce_listening_addr" "true"
        if [ -n "$announced_node_name" ]; then
            escaped_node_name="$(escape_yaml_double_quoted_value "$announced_node_name")"
            update_rendered_config_value_or_exit \
                "$config_file" \
                "fiber" \
                "announced_node_name" \
                "\"$escaped_node_name\""
        else
            remove_config_value_or_exit "$config_file" "fiber" "announced_node_name"
        fi
        update_list_config_value_or_exit "$config_file" "fiber" "announced_addrs" "$announced_addr"
        print_success "Configured this mainnet node as a public Fiber node"
        return
    fi

    update_rendered_config_value_or_exit "$config_file" "fiber" "auto_announce_node" "false"
    update_rendered_config_value_or_exit "$config_file" "fiber" "announce_listening_addr" "false"
    remove_config_value_or_exit "$config_file" "fiber" "announced_node_name"
    update_list_config_value_or_exit "$config_file" "fiber" "announced_addrs"
    print_success "Configured this mainnet node as a non-public Fiber node"
}

rpc_post() {
    local rpc_url="$1"
    local payload="$2"

    if check_command curl; then
        curl -fsSL \
            -H "Content-Type: application/json" \
            -d "$payload" \
            "$rpc_url" 2> /dev/null
    else
        wget -qO- \
            --header="Content-Type: application/json" \
            --post-data="$payload" \
            "$rpc_url" 2> /dev/null
    fi
}

check_ckb_rpc_preflight() {
    local rpc_url
    local expected_genesis_hash
    local response
    local actual_genesis_hash

    rpc_url="$(get_ckb_rpc_url_from_config)"
    if [ -z "$rpc_url" ]; then
        STARTUP_BLOCKER_MESSAGE="Could not read ckb.rpc_url from $INSTALL_DIR/config.yml."
        return 1
    fi

    case "$NETWORK" in
        mainnet)
            expected_genesis_hash="$MAINNET_GENESIS_HASH"
            ;;
        testnet)
            expected_genesis_hash="$TESTNET_GENESIS_HASH"
            ;;
        *)
            STARTUP_BLOCKER_MESSAGE="Unsupported network: $NETWORK"
            return 1
            ;;
    esac

    response="$(rpc_post "$rpc_url" '{"id":2,"jsonrpc":"2.0","method":"get_block_hash","params":["0x0"]}')" || {
        STARTUP_BLOCKER_MESSAGE="Cannot reach the configured CKB RPC at $rpc_url."
        return 1
    }

    actual_genesis_hash="$(printf '%s' "$response" | sed -n 's/.*"result":"\([^"]*\)".*/\1/p')"
    if [ -z "$actual_genesis_hash" ]; then
        STARTUP_BLOCKER_MESSAGE="The CKB RPC at $rpc_url did not return a usable genesis hash."
        return 1
    fi

    if [ "$actual_genesis_hash" != "$expected_genesis_hash" ]; then
        STARTUP_BLOCKER_MESSAGE="The configured CKB RPC at $rpc_url does not appear to be a $NETWORK node."
        return 1
    fi

    return 0
}

install_unix_installer_script() {
    local helper_dir="$INSTALL_DIR/tools/install"
    local local_script_path=""
    local script_url

    mkdir -p "$helper_dir"

    if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
        local_script_path="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
    fi

    if [ -n "$local_script_path" ] && [ -f "$local_script_path" ]; then
        cp "$local_script_path" "$helper_dir/install.sh"
    else
        script_url="${INSTALL_URL:-$INSTALL_URL_DEFAULT}"
        print_info "Downloading Unix installer script..."
        download_file "$script_url" "$helper_dir/install.sh" quiet
    fi

    chmod +x "$helper_dir/install.sh"
    print_success "Unix installer script installed to $helper_dir/install.sh"
}

ensure_ckb_cli_available() {
    local install_mode="$1"

    if CKB_CLI_CMD="$(resolve_existing_ckb_cli_path "$INSTALL_DIR")"; then
        CKB_CLI_HINT_PATH="$CKB_CLI_CMD"
        print_success "ckb-cli found at $CKB_CLI_CMD"
        return
    fi

    require_unzip_if_needed

    if [ "$install_mode" = "bootstrap" ]; then
        print_warning "ckb-cli not found. Downloading it automatically..."
    else
        print_warning "ckb-cli is required but not installed."
        echo ""
        read -p "Would you like to automatically download and install ckb-cli? (y/n): " install_ckb

        if [ "$install_ckb" != "y" ] && [ "$install_ckb" != "Y" ]; then
            print_info "Please install ckb-cli manually:"
            echo "  https://github.com/nervosnetwork/ckb-cli"
            exit 1
        fi
    fi

    install_ckb_cli_binary "$INSTALL_DIR"
    CKB_CLI_CMD="$CKB_CLI_INSTALLED_PATH"
    CKB_CLI_HINT_PATH="$CKB_CLI_INSTALLED_PATH"
}

check_prerequisites() {
    print_info "Checking prerequisites..."

    ensure_download_tool
    print_success "Download tool found"
    ensure_ckb_cli_available guided
}

setup_keys() {
    print_info "Setting up node keys..."

    local ckb_dir="$INSTALL_DIR/ckb"
    mkdir -p "$ckb_dir"

    print_warning "You need a CKB account to run the Fiber node."
    echo ""
    echo "Please choose an option:"
    echo "  1) Create a new CKB account (requires ckb-cli)"
    echo "  2) Use an existing account (you'll need the lock_arg)"
    echo ""
    read -p "Enter your choice (1 or 2): " choice

    case "$choice" in
        1)
            print_info "Creating new CKB account..."
            print_info "You will be prompted to set a password for your CKB wallet"
            echo ""
            # Run ckb-cli account new directly (not capturing output) so it can interact with user
            $CKB_CLI_CMD account new

            if [ $? -ne 0 ]; then
                print_error "Failed to create account"
                exit 1
            fi

            echo ""
            print_info "Account created successfully!"

            # Automatically get the lock_arg from the account list
            print_info "Getting account information..."
            sleep 1

            # Get account list and extract the last created account's lock_arg
            local account_list
            account_list=$($CKB_CLI_CMD account list 2>&1)

            if [ $? -ne 0 ]; then
                print_error "Failed to get account list"
                echo "$account_list"
                exit 1
            fi

            # Extract the most recently added account's lock_arg (the last one in the list)
            LOCK_ARG=$(echo "$account_list" | grep "lock_arg:" | tail -1 | awk '{print $2}')

            if [ -z "$LOCK_ARG" ]; then
                print_error "Could not automatically detect lock_arg"
                print_info "Please check the account list manually:"
                echo "$account_list"
                echo ""
                read -p "Enter the lock_arg manually: " LOCK_ARG

                if [ -z "$LOCK_ARG" ]; then
                    print_error "lock_arg is required"
                    exit 1
                fi
            fi

            print_success "Detected lock_arg: $LOCK_ARG"
            # Save to global variable for summary
            export GLOBAL_LOCK_ARG="$LOCK_ARG"
            ;;
        2)
            echo ""
            read -p "Enter your lock_arg: " LOCK_ARG
            # Save to global variable for summary
            export GLOBAL_LOCK_ARG="$LOCK_ARG"
            ;;
        *)
            print_error "Invalid choice"
            exit 1
            ;;
    esac

    print_info "Exporting private key..."

    # Export the key
    $CKB_CLI_CMD account export --lock-arg "$LOCK_ARG" --extended-privkey-path "$ckb_dir/exported-key"

    # Extract only the private key and remove the exported extended key material.
    head -n 1 "$ckb_dir/exported-key" > "$ckb_dir/key"
    rm -f "$ckb_dir/exported-key"

    print_success "Private key saved to $ckb_dir/key"

    # Set permissions
    chmod 600 "$ckb_dir/key"

    # Show funding information
    show_funding_info "$LOCK_ARG"
}

show_funding_info() {
    local lock_arg="$1"

    echo ""
    printf '%s\n' \
        "${YELLOW}==========================================" \
        "    IMPORTANT: Fund Your Account" \
        "==========================================${NC}"
    echo ""

    # Get the address from ckb-cli
    print_info "Getting your CKB address..."
    local account_info=$($CKB_CLI_CMD account list 2>&1 | grep -A 5 "$lock_arg" || echo "")

    if [ -n "$account_info" ]; then
        echo ""
        echo "Your account addresses:"
        echo "$account_info" | grep -E "mainnet:|testnet:" | head -4
        echo ""
    fi

    echo "To open payment channels and make transactions, you need CKB tokens."
    echo ""

    if [ "$NETWORK" = "testnet" ]; then
        echo "For Testnet:"
    echo "  1. Get free testnet CKB from the faucet:"
    echo "     https://faucet.nervos.org/"
    echo ""
    echo "  2. Check the Nervos documentation for other testnet token sources"
    echo ""
    echo "  3. Send testnet CKB to your testnet address (ckt1...)"
    else
        echo "For Mainnet:"
        echo "  1. Purchase CKB from an exchange (Binance, Coinbase, etc.)"
        echo ""
        echo "  2. Withdraw to your mainnet address (ckb1...)"
        echo ""
        echo "  3. Recommended minimum amount: 1000+ CKB for channel funding"
    fi

    echo ""
    echo "How to check your balance:"
    echo "  ckb-cli wallet get-capacity --lock-arg $lock_arg"
    echo ""
    printf '%s\n' "${YELLOW}Remember: You must have CKB tokens before opening channels!${NC}"
    echo ""
}

create_startup_script() {
    print_info "Creating startup script..."

    cat > "$INSTALL_DIR/start-node.sh" << EOF
#!/bin/bash

# Fiber Network Node Startup Script
# Generated by install.sh

set -e

INSTALL_DIR="\$(cd "\$(dirname "\$0")" && pwd)"
NETWORK_MARKER_FILE="\$INSTALL_DIR/$NETWORK_MARKER_FILE_NAME"
CONFIG_NETWORK=\$(awk '
    /^[^[:space:]#][^:]*:[[:space:]]*$/ {
        in_fiber = (\$0 == "fiber:")
        next
    }

    in_fiber && /^[[:space:]]*chain:[[:space:]]*/ {
        value = \$0
        sub(/^[[:space:]]*chain:[[:space:]]*/, "", value)
        sub(/[[:space:]]+#.*$/, "", value)
        sub(/^"/, "", value)
        sub(/"$/, "", value)
        print value
        exit
    }
' "\$INSTALL_DIR/config.yml")

if [ -f "\$NETWORK_MARKER_FILE" ]; then
    INSTALLED_NETWORK=\$(head -n 1 "\$NETWORK_MARKER_FILE" | tr -d '\r')
    if [ -n "\$CONFIG_NETWORK" ] && [ "\$INSTALLED_NETWORK" != "\$CONFIG_NETWORK" ]; then
        echo "Error: This install directory is marked for \$INSTALLED_NETWORK, but config.yml is set to \$CONFIG_NETWORK."
        echo "Use a separate directory for each network, or remove \$INSTALL_DIR/fiber before switching networks."
        exit 1
    fi
elif [ -n "\$CONFIG_NETWORK" ]; then
    printf '%s\n' "\$CONFIG_NETWORK" > "\$NETWORK_MARKER_FILE"
fi

# Check if password is set
if [ -z "\$FIBER_SECRET_KEY_PASSWORD" ]; then
    echo "Enter your FIBER_SECRET_KEY_PASSWORD:"
    read -s FIBER_SECRET_KEY_PASSWORD
    export FIBER_SECRET_KEY_PASSWORD
    echo ""
fi

echo "Starting Fiber Network Node..."
echo "  Install directory: \$INSTALL_DIR"
echo "  Config file: \$INSTALL_DIR/config.yml"
echo "  Data directory: \$INSTALL_DIR"
echo ""

cd "\$INSTALL_DIR"
RUST_LOG=info ./fnn -c config.yml -d .
EOF

    chmod +x "$INSTALL_DIR/start-node.sh"
    print_success "Startup script created: $INSTALL_DIR/start-node.sh"

    # Show password reminder
    echo ""
    printf '%s\n' \
        "${YELLOW}==========================================" \
        "    IMPORTANT: Save Your Password" \
        "==========================================${NC}"
    echo ""
    echo "The FIBER_SECRET_KEY_PASSWORD is used to encrypt your private key."
    echo "You will need this password EVERY TIME you start the node."
    echo ""
    echo "Options to set the password permanently:"
    echo ""
    echo "1. Set environment variable in your shell profile:"
    echo "   echo 'export FIBER_SECRET_KEY_PASSWORD=\"your-password\"' >> ~/.bashrc"
    echo "   # Or for zsh:"
    echo "   echo 'export FIBER_SECRET_KEY_PASSWORD=\"your-password\"' >> ~/.zshrc"
    echo ""
    echo "2. Edit the start-node.sh script and add the password:"
    echo "   # Open start-node.sh and replace the read prompt with:"
    echo "   export FIBER_SECRET_KEY_PASSWORD=\"your-password\""
    echo ""
    echo "3. Create a wrapper script with the password:"
    echo "   #!/bin/bash"
    echo "   export FIBER_SECRET_KEY_PASSWORD=\"your-password\""
    echo "   ./start-node.sh"
    echo ""
    printf '%s\n' "${RED}⚠️  Security Warning:${NC}"
    echo "   - Never commit passwords to version control"
    echo "   - Keep your password in a secure password manager"
    echo "   - The password cannot be recovered if lost!"
    echo ""
}

prepare_release_bundle() {
    if [ -n "$LOCAL_FNN_BINARY" ]; then
        install_local_fnn_binary "$LOCAL_FNN_BINARY" "$INSTALL_DIR"
    elif [ "$REUSE_EXISTING_INSTALL" -eq 1 ] && has_release_bundle "$INSTALL_DIR"; then
        print_success "Reusing existing release bundle in $INSTALL_DIR"
    else
        install_fnn_binary "$INSTALL_DIR"
    fi

    download_config_file "$INSTALL_DIR" "$NETWORK"
    configure_ckb_rpc_url
    configure_mainnet_public_node
}

print_summary() {
    local can_start_now=1

    echo ""
    printf '%s\n' \
        "${GREEN}==========================================" \
        "    Installation Complete!" \
        "==========================================${NC}"
    echo ""
    echo "Your Fiber Network Node is installed at:"
    echo "  $INSTALL_DIR"
    echo ""
    echo "To start your node, run:"
    echo "  cd $INSTALL_DIR"
    echo "  ./start-node.sh"
    echo ""
    echo "Or manually:"
    echo "  FIBER_SECRET_KEY_PASSWORD='your-password' RUST_LOG=info ./fnn -c config.yml -d ."
    echo ""
    echo "Important files:"
    echo "  - fnn              : Node binary"
    echo "  - fnn-cli          : CLI utility"
    echo "  - fnn-migrate      : Database migration utility"
    echo "  - config/          : Bundled config templates"
    echo "  - config.yml       : Configuration file"
    echo "  - ckb/key          : Private key (KEEP SECURE!)"
    echo "  - start-node.sh    : Easy startup script"
    echo ""
    echo "Documentation:"
    echo "  - https://www.fiber.world/docs"
    echo ""
    print_warning "Remember to:"
    echo "  1. Keep your private key file (ckb/key) secure"
    echo "  2. Use a strong password for FIBER_SECRET_KEY_PASSWORD"
    echo "  3. Backup your ckb/ directory regularly"
    if [ -n "$GLOBAL_LOCK_ARG" ]; then
        echo "  4. Fund your CKB address:"
        echo "     - Get your address: ckb-cli account list | grep '$GLOBAL_LOCK_ARG' -A 5"
        if [ "$NETWORK" = "testnet" ]; then
            echo "     - Testnet faucet: https://faucet.nervos.org/"
        fi
    fi
    echo ""

    if ! check_ckb_rpc_preflight; then
        can_start_now=0
        print_warning "Skipping automatic startup because the configured CKB RPC is not ready."
        echo "  $STARTUP_BLOCKER_MESSAGE"
        echo "  Update ckb.rpc_url in $INSTALL_DIR/config.yml and try again."
        echo ""
    fi

    if [ "$can_start_now" -eq 1 ]; then
        echo ""
        read -p "Would you like to start the node now? (y/n, default: y): " start_now
        start_now=${start_now:-y}

        if [ "$start_now" = "y" ] || [ "$start_now" = "Y" ]; then
            echo ""
            print_info "Starting Fiber Network Node..."
            echo "  Changing to directory: $INSTALL_DIR"
            echo "  Running: ./start-node.sh"
            echo ""
            cd "$INSTALL_DIR"
            ./start-node.sh
            # Exit after node stops (user pressed Ctrl-C or node exited)
            exit 0
        fi
    fi

    echo ""
    print_info "You can start the node later by running:"
    echo "  cd $INSTALL_DIR && ./start-node.sh"
    if [ "$can_start_now" -eq 0 ]; then
        echo "  # First update ckb.rpc_url in config.yml to a reachable $NETWORK CKB RPC"
    fi
    echo ""
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --mode)
                shift
                if [ $# -eq 0 ]; then
                    print_error "--mode requires a value"
                    exit 1
                fi
                INSTALL_MODE="$1"
                ;;
            --local-binary)
                shift
                if [ $# -eq 0 ]; then
                    print_error "--local-binary requires a path"
                    exit 1
                fi
                LOCAL_FNN_BINARY="$1"
                ;;
            -h|--help)
                show_help
                exit 0
                ;;
            --)
                shift
                while [ $# -gt 0 ]; do
                    POSITIONAL_ARGS+=("$1")
                    shift
                done
                break
                ;;
            -*)
                print_error "Unknown option: $1"
                exit 1
                ;;
            *)
                POSITIONAL_ARGS+=("$1")
                ;;
        esac
        shift
    done
}

determine_install_mode() {
    if [ -n "$INSTALL_MODE" ]; then
        case "$INSTALL_MODE" in
            guided|bootstrap)
                return
                ;;
            *)
                print_error "Invalid mode: $INSTALL_MODE. Must be 'guided' or 'bootstrap'"
                exit 1
                ;;
        esac
    fi

    if [ -n "$LOCAL_FNN_BINARY" ]; then
        INSTALL_MODE="guided"
        return
    fi

    if is_interactive_stdin; then
        INSTALL_MODE="guided"
    else
        INSTALL_MODE="bootstrap"
    fi
}

resolve_install_context() {
    determine_install_mode

    case "$INSTALL_MODE" in
        guided)
            INSTALL_DIR="${POSITIONAL_ARGS[0]:-$(resolve_guided_install_dir)}"
            NETWORK="${POSITIONAL_ARGS[1]:-${NETWORK:-}}"
            ;;
        bootstrap)
            INSTALL_DIR="${POSITIONAL_ARGS[0]:-${INSTALL_DIR:-$HOME/.fiber}}"
            NETWORK="${POSITIONAL_ARGS[1]:-${NETWORK:-}}"
            ;;
    esac

    prompt_for_network_if_needed
    detect_platform
}

run_bootstrap_install() {
    print_header

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

    mkdir -p "$INSTALL_DIR"

    ensure_ckb_cli_available bootstrap

    install_fnn_binary "$INSTALL_DIR"
    download_config_file "$INSTALL_DIR" "$NETWORK"
    install_unix_installer_script

    mkdir -p "$INSTALL_DIR/fiber"

    cd "$INSTALL_DIR"

    print_success "Installation complete!"
    echo ""
    echo "Release bundle installed to: $INSTALL_DIR"
    echo "  - $INSTALL_DIR/fnn"
    echo "  - $INSTALL_DIR/fnn-cli"
    echo "  - $INSTALL_DIR/fnn-migrate"
    echo "  - $INSTALL_DIR/config"
    echo "  - $INSTALL_DIR/tools/install/install.sh"
    echo "ckb-cli is available at: $CKB_CLI_HINT_PATH"
    echo ""
    echo "Next steps:"
    echo ""
    echo "For detailed setup instructions:"
    echo "  https://www.fiber.world/docs/quick-start/run-a-node"
    echo ""
    echo "  If you want the guided Unix installer later:"
    echo "     $INSTALL_DIR/tools/install/install.sh $INSTALL_DIR"
}

run_guided_install() {
    print_header

    # Validate network
    validate_network "$NETWORK"
    prepare_install_dir
    ensure_install_dir_matches_network "$INSTALL_DIR" "$NETWORK"

    print_info "Installation directory: $INSTALL_DIR"
    print_info "Network: $NETWORK"
    print_info "Platform: $PLATFORM-$ARCH"
    echo ""

    # Create installation directory
    mkdir -p "$INSTALL_DIR"
    if [ "$REUSE_EXISTING_INSTALL" -eq 1 ]; then
        print_success "Using existing directory: $INSTALL_DIR"
    else
        print_success "Created directory: $INSTALL_DIR"
    fi

    # Check prerequisites
    check_prerequisites

    # Install release bundle and prepare config
    echo ""
    prepare_release_bundle

    # Setup keys
    echo ""
    setup_keys

    # Create startup script
    create_startup_script

    # Print summary
    print_summary
}

main() {
    parse_args "$@"
    resolve_install_context

    case "$INSTALL_MODE" in
        guided)
            run_guided_install
            ;;
        bootstrap)
            run_bootstrap_install
            ;;
    esac
}

main "$@"
