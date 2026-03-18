#!/bin/bash

# Quick Start Script for Fiber Network Node
# This is a simpler alternative to install-fnn.sh for users who already have FNN binary

set -e

INSTALL_DIR="${1:-./my-fnn}"
NETWORK="${2:-testnet}"
CKB_CLI_VERSION="1.12.0"
CKB_CLI_CMD="ckb-cli"

# Platform detection
PLATFORM=$(uname -s)
ARCH=$(uname -m)

echo "=========================================="
echo "    Fiber Network Node Quick Start"
echo "=========================================="
echo ""
echo "This script will:"
echo "  1. Create node directory at: $INSTALL_DIR"
echo "  2. Copy FNN binary from current directory"
echo "  3. Download config file for $NETWORK"
echo "  4. Set up node keys using ckb-cli"
echo ""
read -p "Continue? (y/n): " confirm

if [ "$confirm" != "y" ] && [ "$confirm" != "Y" ]; then
    echo "Aborted."
    exit 0
fi

# Check for fnn binary
if [ ! -f "./fnn" ]; then
    echo "Error: fnn binary not found in current directory"
    echo "Please download or build fnn first, then run this script from the same directory"
    exit 1
fi

# Check for curl or wget
if ! command -v curl &> /dev/null && ! command -v wget &> /dev/null; then
    echo "Error: Neither curl nor wget found"
    exit 1
fi

# Check for ckb-cli and install if needed
install_ckb_cli() {
    echo ""
    echo "ckb-cli is required but not installed."
    read -p "Would you like to automatically download and install ckb-cli? (y/n): " install_ckb
    
    if [ "$install_ckb" != "y" ] && [ "$install_ckb" != "Y" ]; then
        echo "Please install ckb-cli manually:"
        echo "  https://github.com/nervosnetwork/ckb-cli"
        exit 1
    fi
    
    echo "Downloading ckb-cli v${CKB_CLI_VERSION}..."
    
    # Determine ckb-cli binary name
    case "$PLATFORM" in
        Linux*)
            case "$ARCH" in
                x86_64) CKB_CLI_BINARY="ckb-cli_v${CKB_CLI_VERSION}_x86_64-unknown-linux-gnu.tar.gz" ;;
                aarch64) CKB_CLI_BINARY="ckb-cli_v${CKB_CLI_VERSION}_aarch64-unknown-linux-gnu.tar.gz" ;;
                *) echo "Error: Unsupported architecture: $ARCH"; exit 1 ;;
            esac
            ;;
        Darwin*)
            case "$ARCH" in
                x86_64) CKB_CLI_BINARY="ckb-cli_v${CKB_CLI_VERSION}_x86_64-apple-darwin.zip" ;;
                arm64) CKB_CLI_BINARY="ckb-cli_v${CKB_CLI_VERSION}_aarch64-apple-darwin.zip" ;;
                *) echo "Error: Unsupported architecture: $ARCH"; exit 1 ;;
            esac
            if ! command -v unzip &> /dev/null; then
                echo "Error: unzip is required on macOS. Install with: brew install unzip"
                exit 1
            fi
            ;;
        *)
            echo "Error: Unsupported platform: $PLATFORM"
            exit 1
            ;;
    esac
    
    local download_url="https://github.com/nervosnetwork/ckb-cli/releases/download/v${CKB_CLI_VERSION}/${CKB_CLI_BINARY}"
    local temp_dir=$(mktemp -d)
    
    echo "  Downloading from: $download_url"
    
    if command -v curl &> /dev/null; then
        curl -L -o "${temp_dir}/${CKB_CLI_BINARY}" "$download_url" --progress-bar
    else
        wget -O "${temp_dir}/${CKB_CLI_BINARY}" "$download_url" --show-progress
    fi
    
    echo "Extracting ckb-cli..."
    cd "$temp_dir"
    case "$CKB_CLI_BINARY" in
        *.tar.gz) tar -xzf "${temp_dir}/${CKB_CLI_BINARY}" ;;
        *.zip) unzip -q "${temp_dir}/${CKB_CLI_BINARY}" ;;
    esac
    
    CKB_CLI_PATH=$(find "$temp_dir" -name "ckb-cli" -type f | head -1)
    if [ -z "$CKB_CLI_PATH" ]; then
        echo "Error: Could not find ckb-cli binary"
        rm -rf "$temp_dir"
        exit 1
    fi
    
    chmod +x "$CKB_CLI_PATH"
    
    # Install to install directory
    mkdir -p "$INSTALL_DIR"
    cp "$CKB_CLI_PATH" "$INSTALL_DIR/ckb-cli"
    CKB_CLI_CMD="$INSTALL_DIR/ckb-cli"
    echo "✓ ckb-cli installed to $INSTALL_DIR/ckb-cli"
    
    rm -rf "$temp_dir"
    cd - > /dev/null
}

if ! command -v ckb-cli &> /dev/null; then
    # Check if ckb-cli exists in install directory
    if [ -f "$INSTALL_DIR/ckb-cli" ]; then
        echo "Found ckb-cli in install directory"
        CKB_CLI_CMD="$INSTALL_DIR/ckb-cli"
    else
        install_ckb_cli
    fi
else
    CKB_CLI_CMD="$(command -v ckb-cli)"
    echo "✓ ckb-cli found"
fi

# Create directory
mkdir -p "$INSTALL_DIR"
mkdir -p "$INSTALL_DIR/ckb"

# Copy binary
cp ./fnn "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/fnn"
echo "✓ Copied fnn binary"

# Download config
if command -v curl &> /dev/null; then
    curl -L -o "$INSTALL_DIR/config.yml" "https://raw.githubusercontent.com/nervosnetwork/fiber/v0.6.1/config/${NETWORK}/config.yml" -s
elif command -v wget &> /dev/null; then
    wget -O "$INSTALL_DIR/config.yml" "https://raw.githubusercontent.com/nervosnetwork/fiber/v0.6.1/config/${NETWORK}/config.yml" -q
else
    echo "Error: Neither curl nor wget found"
    exit 1
fi
echo "✓ Downloaded config.yml"

# Setup keys
echo ""
echo "Setting up node keys..."
echo ""
echo "Choose an option:"
echo "  1) Create new CKB account"
echo "  2) Use existing account (need lock_arg)"
read -p "Enter choice (1 or 2): " choice

case "$choice" in
    1)
        echo "Creating new CKB account..."
        echo "You will be prompted to set a password for your CKB wallet"
        echo ""
        # Run ckb-cli account new directly (not capturing output) so it can interact with user
        $CKB_CLI_CMD account new
        
        if [ $? -ne 0 ]; then
            echo "Error: Failed to create account"
            exit 1
        fi
        
        echo ""
        echo "Account created successfully!"
        
        # Automatically get the lock_arg from the account list
        echo "Getting account information..."
        sleep 1
        
        # Get account list and extract the last created account's lock_arg
        ACCOUNT_LIST=$($CKB_CLI_CMD account list 2>&1)
        
        if [ $? -ne 0 ]; then
            echo "Error: Failed to get account list"
            echo "$ACCOUNT_LIST"
            exit 1
        fi
        
        # Extract the most recently added account's lock_arg (the last one in the list)
        LOCK_ARG=$(echo "$ACCOUNT_LIST" | grep "lock_arg:" | tail -1 | awk '{print $2}')
        
        if [ -z "$LOCK_ARG" ]; then
            echo "Error: Could not automatically detect lock_arg"
            echo "Please check the account list manually:"
            echo "$ACCOUNT_LIST"
            echo ""
            read -p "Enter the lock_arg manually: " LOCK_ARG
            
            if [ -z "$LOCK_ARG" ]; then
                echo "Error: lock_arg is required"
                exit 1
            fi
        fi
        
        echo "✓ Detected lock_arg: $LOCK_ARG"
        ;;
    2)
        read -p "Enter lock_arg: " LOCK_ARG
        ;;
    *)
        echo "Invalid choice"
        exit 1
        ;;
esac

# Export key
cd "$INSTALL_DIR"
$CKB_CLI_CMD account export --lock-arg "$LOCK_ARG" --extended-privkey-path ./ckb/exported-key
head -n 1 ./ckb/exported-key > ./ckb/key
chmod 600 ./ckb/key
chmod 600 ./ckb/exported-key
echo "✓ Private key exported"

# Show funding information
echo ""
echo "=========================================="
echo "    IMPORTANT: Fund Your Account"
echo "=========================================="
echo ""

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
echo "  $CKB_CLI_CMD wallet get-capacity --lock-arg $LOCK_ARG"
echo ""
echo "⚠ Remember: You must have CKB tokens before opening channels!"
echo ""

# Create start script
cat > ./start-node.sh << 'EOF'
#!/bin/bash
cd "$(dirname "$0")"
if [ -z "$FIBER_SECRET_KEY_PASSWORD" ]; then
    echo "Enter FIBER_SECRET_KEY_PASSWORD:"
    read -s FIBER_SECRET_KEY_PASSWORD
    export FIBER_SECRET_KEY_PASSWORD
    echo ""
fi
RUST_LOG=info ./fnn -c config.yml -d .
EOF
chmod +x ./start-node.sh
echo "✓ Created start-node.sh"

# Show password reminder
echo ""
echo "=========================================="
echo "    IMPORTANT: Save Your Password"
echo "=========================================="
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
echo "   # Replace the read prompt with:"
echo "   export FIBER_SECRET_KEY_PASSWORD=\"your-password\""
echo ""
echo "3. Create a wrapper script with the password:"
echo "   #!/bin/bash"
echo "   export FIBER_SECRET_KEY_PASSWORD=\"your-password\""
echo "   ./start-node.sh"
echo ""
echo "⚠️  Security Warning:"
echo "   - Never commit passwords to version control"
echo "   - Keep your password in a secure password manager"
echo "   - The password cannot be recovered if lost!"
echo ""

echo ""
echo "=========================================="
echo "    Setup Complete!"
echo "=========================================="
echo ""
echo "To start your node:"
echo "  cd $INSTALL_DIR"
echo "  ./start-node.sh"
echo ""
echo "Or:"
echo "  FIBER_SECRET_KEY_PASSWORD='your-password' RUST_LOG=info ./fnn -c config.yml -d ."
echo ""

# Ask if user wants to start the node now
echo ""
read -p "Would you like to start the node now? (y/n, default: y): " start_now
start_now=${start_now:-y}

if [ "$start_now" = "y" ] || [ "$start_now" = "Y" ]; then
    echo ""
    echo "Starting Fiber Network Node..."
    echo "  Changing to directory: $INSTALL_DIR"
    echo "  Running: ./start-node.sh"
    echo ""
    cd "$INSTALL_DIR"
    ./start-node.sh
    # Exit after node stops (user pressed Ctrl-C or node exited)
    exit 0
else
    echo ""
    echo "You can start the node later by running:"
    echo "  cd $INSTALL_DIR && ./start-node.sh"
    echo ""
fi
