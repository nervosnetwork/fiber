# Fiber Network Node (FNN) Installation Guide

## Quick Install (One-liner)

```bash
# Install to default location (~/.fiber)
curl -sSfL https://get.fiber.world | sh

# Install to custom location
curl -sSfL https://get.fiber.world | INSTALL_DIR=/opt/fiber sh

# Install specific version
curl -sSfL https://get.fiber.world | FNN_VERSION=0.7.1 sh

# Install for mainnet
curl -sSfL https://get.fiber.world | NETWORK=mainnet sh
```

## Setup Instructions

### 1. Prerequisites

- Linux or macOS (x86_64 or ARM64)
- `curl` or `wget`
- `ckb-cli` (will be installed if not present)

### 2. Quick Start

```bash
# 1. Install FNN
curl -sSfL https://get.fiber.world | sh

# 2. Create CKB account (if you don't have one)
ckb-cli account new

# 3. Export your private key
ckb-cli account export --lock-arg <your-lock-arg> --extended-privkey-path ~/.fiber/ckb/exported-key
head -1 ~/.fiber/ckb/exported-key > ~/.fiber/ckb/key

# 4. Set password (required)
export FIBER_SECRET_KEY_PASSWORD="your-secure-password"

# 5. Start the node
~/.fiber/fnn -c ~/.fiber/config.yml -d ~/.fiber
```

### 3. Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `INSTALL_DIR` | Installation directory | `~/.fiber` |
| `FNN_VERSION` | Version to install | `0.7.1` |
| `NETWORK` | Network (testnet/mainnet) | `testnet` |
| `FIBER_SECRET_KEY_PASSWORD` | Password for key encryption | (required) |

### 4. Post-Installation

After installation, you'll need to:

1. **Fund your account** (for testnet):
   - Get testnet CKB from https://faucet.nervos.org/
   - Check your address: `ckb-cli account list`

2. **Set up auto-start** (optional):
   ```bash
   # Create systemd service (Linux)
   sudo tee /etc/systemd/system/fnn.service > /dev/null <<EOF
   [Unit]
   Description=Fiber Network Node
   After=network.target

   [Service]
   Type=simple
   User=$USER
   Environment=FIBER_SECRET_KEY_PASSWORD=your-password
   Environment=RUST_LOG=info
   ExecStart=$HOME/.fiber/fnn -c $HOME/.fiber/config.yml -d $HOME/.fiber
   Restart=on-failure

   [Install]
   WantedBy=multi-user.target
   EOF

   sudo systemctl enable fnn
   sudo systemctl start fnn
   ```

3. **Configure your node**:
   Edit `~/.fiber/config.yml` to customize:
   - Listening address and port
   - RPC settings
   - UDT whitelist

## Troubleshooting

### Permission Denied

```bash
chmod +x ~/.fiber/fnn
```

### Missing ckb-cli

```bash
# macOS
brew install ckb-cli

# Linux
# Download from https://github.com/nervosnetwork/ckb-cli/releases
```

### Port Already in Use

Edit `~/.fiber/config.yml` and change the listening port:
```yaml
fiber:
  listening_addr: "/ip4/0.0.0.0/tcp/8234"  # Change 8234 to another port
```

## Upgrading

```bash
# Backup your data first
cp -r ~/.fiber/fiber ~/.fiber/fiber.backup

# Reinstall with new version
curl -sSfL https://get.fiber.world | FNN_VERSION=0.8.0 sh
```

## Uninstalling

```bash
rm -rf ~/.fiber
# Also remove any systemd services if created
```

## Security Notes

⚠️ **Important**:
- Never share your `~/.fiber/ckb/key` file
- Keep your `FIBER_SECRET_KEY_PASSWORD` secure
- Backup your `~/.fiber/ckb/` directory regularly
- The password cannot be recovered if lost!

## Documentation

- [Fiber Docs](https://docs.fiber.world/)
- [GitHub](https://github.com/nervosnetwork/fiber)
- [RPC API](https://github.com/nervosnetwork/fiber/blob/main/src/rpc/README.md)
