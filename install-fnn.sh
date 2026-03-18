#!/bin/bash

# Fiber Network Node (FNN) Installer Script
# Usage: ./install-fnn.sh [install-directory] [network]
# Example: ./install-fnn.sh ~/my-fiber-node testnet

set -e

# Global variable for ckb-cli command
CKB_CLI_CMD="ckb-cli"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
FNN_VERSION="0.6.1"
CKB_CLI_VERSION="1.12.0"
INSTALL_DIR="${1:-./my-fnn}"
NETWORK="${2:-testnet}"
GITHUB_RELEASE_URL="https://github.com/nervosnetwork/fiber/releases/download/v${FNN_VERSION}"
CKB_CLI_RELEASE_URL="https://github.com/nervosnetwork/ckb-cli/releases/download/v${CKB_CLI_VERSION}"

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
        # macOS ARM64 (M1/M2/M3) uses aarch64 builds
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
    if command -v "$1" &> /dev/null; then
        return 0
    else
        return 1
    fi
}

install_ckb_cli() {
    print_warning "ckb-cli is required but not installed."
    echo ""
    read -p "Would you like to automatically download and install ckb-cli? (y/n): " install_ckb
    
    if [ "$install_ckb" != "y" ] && [ "$install_ckb" != "Y" ]; then
        print_info "Please install ckb-cli manually:"
        echo "  https://github.com/nervosnetwork/ckb-cli"
        exit 1
    fi
    
    print_info "Downloading ckb-cli v${CKB_CLI_VERSION}..."
    
    # Determine ckb-cli binary name based on platform
    case "$PLATFORM" in
        Linux*)
            case "$ARCH" in
                x86_64) CKB_CLI_BINARY="ckb-cli_v${CKB_CLI_VERSION}_x86_64-unknown-linux-gnu.tar.gz" ;;
                aarch64) CKB_CLI_BINARY="ckb-cli_v${CKB_CLI_VERSION}_aarch64-unknown-linux-gnu.tar.gz" ;;
                *) print_error "Unsupported architecture for ckb-cli: $ARCH"; exit 1 ;;
            esac
            ;;
        Darwin*)
            case "$ARCH" in
                x86_64) CKB_CLI_BINARY="ckb-cli_v${CKB_CLI_VERSION}_x86_64-apple-darwin.zip" ;;
                arm64) CKB_CLI_BINARY="ckb-cli_v${CKB_CLI_VERSION}_aarch64-apple-darwin.zip" ;;
                *) print_error "Unsupported architecture for ckb-cli: $ARCH"; exit 1 ;;
            esac
            ;;
    esac
    
    local download_url="${CKB_CLI_RELEASE_URL}/${CKB_CLI_BINARY}"
    local temp_dir=$(mktemp -d)
    
    echo "  Downloading from: $download_url"
    
    if check_command curl; then
        curl -L -o "${temp_dir}/${CKB_CLI_BINARY}" "$download_url" --progress-bar
    else
        wget -O "${temp_dir}/${CKB_CLI_BINARY}" "$download_url" --show-progress
    fi
    
    print_info "Extracting ckb-cli..."
    
    cd "$temp_dir"
    case "$CKB_CLI_BINARY" in
        *.tar.gz)
            tar -xzf "${temp_dir}/${CKB_CLI_BINARY}"
            ;;
        *.zip)
            unzip -q "${temp_dir}/${CKB_CLI_BINARY}"
            ;;
    esac
    
    # Find ckb-cli binary
    CKB_CLI_PATH=$(find "$temp_dir" -name "ckb-cli" -type f | head -1)
    
    if [ -z "$CKB_CLI_PATH" ]; then
        print_error "Could not find ckb-cli binary in the downloaded archive"
        rm -rf "$temp_dir"
        exit 1
    fi
    
    chmod +x "$CKB_CLI_PATH"
    
    # Install to appropriate location
    if [ -w "/usr/local/bin" ]; then
        cp "$CKB_CLI_PATH" /usr/local/bin/
        CKB_CLI_CMD="/usr/local/bin/ckb-cli"
        print_success "ckb-cli installed to /usr/local/bin/"
    elif [ -d "$HOME/.local/bin" ]; then
        mkdir -p "$HOME/.local/bin"
        cp "$CKB_CLI_PATH" "$HOME/.local/bin/"
        CKB_CLI_CMD="$HOME/.local/bin/ckb-cli"
        print_success "ckb-cli installed to $HOME/.local/bin/"
        print_warning "Please ensure $HOME/.local/bin is in your PATH"
    else
        # Install to install directory
        cp "$CKB_CLI_PATH" "$INSTALL_DIR/ckb-cli"
        CKB_CLI_CMD="$INSTALL_DIR/ckb-cli"
        print_success "ckb-cli installed to $INSTALL_DIR/ckb-cli"
        print_warning "To use ckb-cli from command line, add $INSTALL_DIR to your PATH:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    fi
    
    rm -rf "$temp_dir"
    cd - > /dev/null
}

check_prerequisites() {
    print_info "Checking prerequisites..."
    
    # Check for curl or wget
    if ! check_command curl && ! check_command wget; then
        print_error "Neither curl nor wget is installed. Please install one of them."
        exit 1
    fi
    print_success "Download tool found"
    
    # Check for unzip on macOS (needed for ckb-cli)
    if [ "$PLATFORM" = "Darwin" ] && ! check_command unzip; then
        print_error "unzip is required on macOS. Please install it:"
        echo "  brew install unzip"
        exit 1
    fi
    
    # Check for ckb-cli
    if ! check_command ckb-cli; then
        # Check if ckb-cli exists in install directory
        if [ -f "$INSTALL_DIR/ckb-cli" ]; then
            print_info "Found ckb-cli in install directory"
            CKB_CLI_CMD="$INSTALL_DIR/ckb-cli"
        else
            install_ckb_cli
        fi
    else
        print_success "ckb-cli found"
        CKB_CLI_CMD="$(command -v ckb-cli)"
    fi
}

download_binary() {
    if [ -n "$BUILD_FROM_SOURCE" ]; then
        print_info "No pre-built binary available for ${PLATFORM}-${ARCH}"
        return 1
    fi
    
    print_info "Downloading FNN binary v${FNN_VERSION} for ${PLATFORM}-${ARCH}..."
    
    local download_url="${GITHUB_RELEASE_URL}/${BINARY_NAME}"
    local temp_dir=$(mktemp -d)
    
    echo "  Downloading from: $download_url"
    
    if check_command curl; then
        curl -L -o "${temp_dir}/${BINARY_NAME}" "$download_url" --progress-bar
    else
        wget -O "${temp_dir}/${BINARY_NAME}" "$download_url" --show-progress
    fi
    
    print_success "Download completed"
    
    # Extract binary
    print_info "Extracting binary..."
    tar -xzf "${temp_dir}/${BINARY_NAME}" -C "$temp_dir"
    
    # Find fnn binary
    FNN_BINARY_PATH=$(find "$temp_dir" -name "fnn" -type f | head -1)
    
    if [ -z "$FNN_BINARY_PATH" ]; then
        print_error "Could not find fnn binary in the downloaded archive"
        exit 1
    fi
    
    # Make executable and move to install dir
    chmod +x "$FNN_BINARY_PATH"
    cp "$FNN_BINARY_PATH" "$INSTALL_DIR/"
    
    # Cleanup
    rm -rf "$temp_dir"
    
    print_success "Binary installed to $INSTALL_DIR/fnn"
}

download_config() {
    print_info "Downloading configuration files..."
    
    local config_url="https://raw.githubusercontent.com/nervosnetwork/fiber/v${FNN_VERSION}/config/${NETWORK}/config.yml"
    
    if check_command curl; then
        curl -L -o "$INSTALL_DIR/config.yml" "$config_url" -s
    else
        wget -O "$INSTALL_DIR/config.yml" "$config_url" -q
    fi
    
    print_success "Configuration downloaded"
}

build_from_source() {
    print_warning "Building from source instead..."
    print_info "This requires Rust to be installed."
    
    if ! check_command cargo; then
        print_error "Rust/Cargo is not installed. Please install Rust first:"
        echo "  https://www.rust-lang.org/tools/install"
        exit 1
    fi
    
    local temp_dir=$(mktemp -d)
    
    print_info "Cloning repository..."
    git clone --depth 1 --branch "v${FNN_VERSION}" https://github.com/nervosnetwork/fiber.git "$temp_dir"
    
    print_info "Building FNN (this may take several minutes)..."
    cd "$temp_dir"
    cargo build --release
    
    cp "target/release/fnn" "$INSTALL_DIR/"
    cp "config/${NETWORK}/config.yml" "$INSTALL_DIR/"
    
    cd - > /dev/null
    rm -rf "$temp_dir"
    
    print_success "Build completed"
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
    
    # Extract just the private key
    head -n 1 "$ckb_dir/exported-key" > "$ckb_dir/key"
    
    print_success "Private key saved to $ckb_dir/key"
    
    # Set permissions
    chmod 600 "$ckb_dir/key"
    chmod 600 "$ckb_dir/exported-key"
    
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
    
    # Get the password that was set during installation (if any)
    local saved_password="${FIBER_SECRET_KEY_PASSWORD:-}"
    
    cat > "$INSTALL_DIR/start-node.sh" << EOF
#!/bin/bash

# Fiber Network Node Startup Script
# Generated by install-fnn.sh

set -e

INSTALL_DIR="\$(cd "\$(dirname "\$0")" && pwd)"

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

create_readme() {
    cat > "$INSTALL_DIR/README.md" << EOF
# Fiber Network Node

This directory contains your Fiber Network Node installation.

## Files and Directories

- \`fnn\` - The Fiber Network Node binary
- \`config.yml\` - Node configuration file
- \`ckb/key\` - Your private key file (keep this secure!)
- \`start-node.sh\` - Script to start the node
- \`fiber/\` - Node data directory (created on first run)

## Quick Start

1. Start the node:
   \`\`\`bash
   ./start-node.sh
   \`\`\`

   Or manually:
   \`\`\`bash
   FIBER_SECRET_KEY_PASSWORD='your-password' RUST_LOG=info ./fnn -c config.yml -d .
   \`\`\`

2. The node will start syncing with the ${NETWORK}.

3. Check the logs for connection status.

## Password Setup (Important!)

The \`FIBER_SECRET_KEY_PASSWORD\` is required every time you start the node.

### Option 1: Set environment variable (Recommended)

Add to your shell profile (\`~/.bashrc\`, \`~/.zshrc\`, etc.):
\`\`\`bash
export FIBER_SECRET_KEY_PASSWORD="your-password"
\`\`\`

Then reload: \`source ~/.bashrc\` or \`source ~/.zshrc\`

### Option 2: Edit the startup script

Edit \`start-node.sh\` and set the password directly:
\`\`\`bash
# Replace the read prompt with:
export FIBER_SECRET_KEY_PASSWORD="your-password"
\`\`\`

### Option 3: Create a wrapper script

Create \`start-with-password.sh\`:
\`\`\`bash
#!/bin/bash
export FIBER_SECRET_KEY_PASSWORD="your-password"
./start-node.sh
\`\`\`

**⚠️ Security Warning:**
- Never commit passwords to version control
- Use a secure password manager
- The password cannot be recovered if lost!

## Configuration

Edit \`config.yml\` to customize:
- Listening address and port
- RPC settings
- CKB node URL
- UDT whitelist

## Security Notes

- Never share your \`ckb/key\` file
- Keep your FIBER_SECRET_KEY_PASSWORD secure
- The \`ckb/\` directory contains sensitive data - back it up safely

## Upgrading

To upgrade to a new version:
1. Stop the node
2. Backup your data: \`cp -r fiber/store fiber/store.backup\`
3. Download new binary and replace \`fnn\`
4. Start the node again

## Documentation

- Fiber Docs: https://docs.fiber.world/
- GitHub: https://github.com/nervosnetwork/fiber
- RPC API: https://github.com/nervosnetwork/fiber/blob/main/src/rpc/README.md
EOF

    print_success "README created"
}

print_summary() {
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
    echo "  - config.yml       : Configuration file"
    echo "  - ckb/key          : Private key (KEEP SECURE!)"
    echo "  - start-node.sh    : Easy startup script"
    echo ""
    echo "Documentation:"
    echo "  - https://docs.fiber.world/"
    echo "  - https://github.com/nervosnetwork/fiber"
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
    
    # Ask if user wants to start the node now
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
    else
        echo ""
        print_info "You can start the node later by running:"
        echo "  cd $INSTALL_DIR && ./start-node.sh"
        echo ""
    fi
}

main() {
    print_header
    
    # Validate network
    if [ "$NETWORK" != "testnet" ] && [ "$NETWORK" != "mainnet" ]; then
        print_error "Invalid network: $NETWORK. Must be 'testnet' or 'mainnet'"
        exit 1
    fi
    
    print_info "Installation directory: $INSTALL_DIR"
    print_info "Network: $NETWORK"
    print_info "Platform: $PLATFORM-$ARCH"
    echo ""
    
    # Check prerequisites
    check_prerequisites
    
    # Create installation directory
    mkdir -p "$INSTALL_DIR"
    print_success "Created directory: $INSTALL_DIR"
    
    # Download or build binary
    echo ""
    read -p "Download pre-built binary? (y/n, default: y): " download_choice
    download_choice=${download_choice:-y}
    
    if [ "$download_choice" = "y" ] || [ "$download_choice" = "Y" ]; then
        download_binary
        download_config
    else
        build_from_source
    fi
    
    # Setup keys
    echo ""
    setup_keys
    
    # Create startup script
    create_startup_script
    
    # Create README
    create_readme
    
    # Print summary
    print_summary
}

# Run main function
main "$@"
