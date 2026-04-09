#!/bin/bash

# Fiber Network Node (FNN) Quick Start Script
# Usage: ./tools/install/quick-start.sh [install-directory] [network]
# Example: ./tools/install/quick-start.sh ~/my-fiber-node mainnet

set -e

# shellcheck source=install-common.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/install-common.sh"
init_install_defaults

# Global variable for ckb-cli command
CKB_CLI_CMD="ckb-cli"
LOCAL_FNN_BINARY=""
MAINNET_GENESIS_HASH="0x92b197aa1fba0f63633922c61c92375c9c074a93e85963554f5499fe1450d0e5"
TESTNET_GENESIS_HASH="0x10639e0895502b5688a6be8cf69460d76541bfa4821629d86d62ba0aae3f9606"
STARTUP_BLOCKER_MESSAGE=""
REUSE_EXISTING_INSTALL=0
PUBLIC_NODE_ANNOUNCED_ADDR_PLACEHOLDER="/ip4/YOUR-FIBER-NODE-PUBLIC-IP/tcp/8228"
PUBLIC_NODE_NAME_PLACEHOLDER="my-fiber-node"

show_help() {
    cat <<EOF
Fiber Network Node (FNN) quick start

Usage:
  ./tools/install/quick-start.sh [install-directory] [network]
  ./tools/install/quick-start.sh --local-binary /path/to/fnn [install-directory] [network]

Options:
  --local-binary PATH   Use an existing local fnn binary instead of downloading the release bundle.
  -h, --help            Show this help message.

Behavior:
  - If quick-start runs from an existing installed bundle, it reuses that install directory and bundle by default.
  - If the install directory already has contents, quick-start backs it up by default.
  - You can also enter a different install directory path when prompted.
  - If no network is provided, quick-start defaults to mainnet.
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

resolve_default_install_dir() {
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

    mv "$install_dir" "$backup_dir"
    print_success "Backed up existing install path to $backup_dir"
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
    local current_rpc_url
    local desired_rpc_url

    current_rpc_url="$(get_ckb_rpc_url_from_config)"
    desired_rpc_url="${CKB_RPC_URL:-$current_rpc_url}"

    if [ -n "${CKB_RPC_URL:-}" ]; then
        if ! set_config_value_in_section "$INSTALL_DIR/config.yml" "ckb" "rpc_url" "$CKB_RPC_URL"; then
            print_error "Failed to update ckb.rpc_url in $INSTALL_DIR/config.yml"
            exit 1
        fi
        print_success "Configured CKB RPC URL: $CKB_RPC_URL"
        return
    fi

    if [ "$NETWORK" != "mainnet" ] || ! is_interactive_stdin; then
        return
    fi

    echo ""
    print_warning "Mainnet requires a reachable CKB RPC endpoint."
    echo "  Press Enter to use the default public RPC, or provide your own trusted endpoint."
    echo "  Current ckb.rpc_url: $current_rpc_url"
    read -p "Enter the CKB RPC URL to use (press Enter to keep the current value): " desired_rpc_url
    desired_rpc_url="${desired_rpc_url:-$current_rpc_url}"

    if ! set_config_value_in_section "$INSTALL_DIR/config.yml" "ckb" "rpc_url" "$desired_rpc_url"; then
        print_error "Failed to update ckb.rpc_url in $INSTALL_DIR/config.yml"
        exit 1
    fi
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

        set_rendered_config_value_in_section "$config_file" "fiber" "auto_announce_node" "true" || {
            print_error "Failed to update fiber.auto_announce_node in $config_file"
            exit 1
        }
        set_rendered_config_value_in_section "$config_file" "fiber" "announce_listening_addr" "true" || {
            print_error "Failed to update fiber.announce_listening_addr in $config_file"
            exit 1
        }
        if [ -n "$announced_node_name" ]; then
            escaped_node_name="$(escape_yaml_double_quoted_value "$announced_node_name")"
            set_rendered_config_value_in_section "$config_file" "fiber" "announced_node_name" "\"$escaped_node_name\"" || {
                print_error "Failed to update fiber.announced_node_name in $config_file"
                exit 1
            }
        else
            remove_config_value_in_section "$config_file" "fiber" "announced_node_name" || {
                print_error "Failed to update fiber.announced_node_name in $config_file"
                exit 1
            }
        fi
        set_list_value_in_section "$config_file" "fiber" "announced_addrs" "$announced_addr" || {
            print_error "Failed to update fiber.announced_addrs in $config_file"
            exit 1
        }
        print_success "Configured this mainnet node as a public Fiber node"
        return
    fi

    set_rendered_config_value_in_section "$config_file" "fiber" "auto_announce_node" "false" || {
        print_error "Failed to update fiber.auto_announce_node in $config_file"
        exit 1
    }
    set_rendered_config_value_in_section "$config_file" "fiber" "announce_listening_addr" "false" || {
        print_error "Failed to update fiber.announce_listening_addr in $config_file"
        exit 1
    }
    remove_config_value_in_section "$config_file" "fiber" "announced_node_name" || {
        print_error "Failed to update fiber.announced_node_name in $config_file"
        exit 1
    }
    set_list_value_in_section "$config_file" "fiber" "announced_addrs" || {
        print_error "Failed to update fiber.announced_addrs in $config_file"
        exit 1
    }
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

POSITIONAL_ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
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

INSTALL_DIR="${POSITIONAL_ARGS[0]:-$(resolve_default_install_dir)}"
NETWORK="${POSITIONAL_ARGS[1]:-${NETWORK:-}}"
prompt_for_network_if_needed
detect_platform

install_ckb_cli() {
    print_warning "ckb-cli is required but not installed."
    echo ""
    read -p "Would you like to automatically download and install ckb-cli? (y/n): " install_ckb

    if [ "$install_ckb" != "y" ] && [ "$install_ckb" != "Y" ]; then
        print_info "Please install ckb-cli manually:"
        echo "  https://github.com/nervosnetwork/ckb-cli"
        exit 1
    fi

    install_ckb_cli_binary "$INSTALL_DIR"
    CKB_CLI_CMD="$CKB_CLI_INSTALLED_PATH"
}

check_prerequisites() {
    print_info "Checking prerequisites..."

    ensure_download_tool
    print_success "Download tool found"

    require_unzip_if_needed

    if CKB_CLI_CMD="$(resolve_existing_ckb_cli_path "$INSTALL_DIR")"; then
        print_success "ckb-cli found at $CKB_CLI_CMD"
        return
    fi

    install_ckb_cli
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
    echo -e "${YELLOW}=========================================="
    echo "    IMPORTANT: Fund Your Account"
    echo "==========================================${NC}"
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
    echo -e "${YELLOW}Remember: You must have CKB tokens before opening channels!${NC}"
    echo ""
}

create_startup_script() {
    print_info "Creating startup script..."

    cat > "$INSTALL_DIR/start-node.sh" << EOF
#!/bin/bash

# Fiber Network Node Startup Script
# Generated by quick-start.sh

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
    echo -e "${YELLOW}=========================================="
    echo "    IMPORTANT: Save Your Password"
    echo "==========================================${NC}"
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
    echo -e "${RED}⚠️  Security Warning:${NC}"
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
    echo -e "${GREEN}=========================================="
    echo "    Installation Complete!"
    echo "==========================================${NC}"
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

main() {
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

# Run main function
main "$@"
