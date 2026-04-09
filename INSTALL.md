# Fiber Network Node (FNN) Installation Guide

## Quick Install (One-liner)

```bash
# Install to default location (~/.fiber)
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | bash

# Install to custom location
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | INSTALL_DIR=/opt/fiber bash

# Install specific version
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | FNN_VERSION=0.8.0 bash

# Install for mainnet
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | NETWORK=mainnet bash
```

If you need to test an unpublished public branch or fork, replace `main` in the GitHub Raw URL, or set `INSTALL_REPO` and `INSTALL_REF` before `bash`.
The examples below assume these installer files have already been published on the upstream `main` branch.

## Setup Instructions

### Quick Start

```bash
# 1. Install FNN
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | bash

# 2. Create CKB account (if you don't have one)
ckb-cli account new

# 3. Export your private key
ckb-cli account export --lock-arg <your-lock-arg> --extended-privkey-path ~/.fiber/ckb/exported-key
head -1 ~/.fiber/ckb/exported-key > ~/.fiber/ckb/key
rm ~/.fiber/ckb/exported-key

# 4. Set password (required)
export FIBER_SECRET_KEY_PASSWORD="your-secure-password"

# 5. Start the node
~/.fiber/fnn -c ~/.fiber/config.yml -d ~/.fiber
```

The installed release bundle also includes:

- `~/.fiber/fnn-cli`
- `~/.fiber/fnn-migrate`
- `~/.fiber/config/`
- `~/.fiber/tools/install/quick-start.sh`

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `INSTALL_DIR` | Installation directory | `~/.fiber` |
| `FNN_VERSION` | Version to install | `0.8.0` |
| `NETWORK` | Network (testnet/mainnet) | `testnet` |
| `FIBER_SECRET_KEY_PASSWORD` | Password for key encryption | (required) |

For `NETWORK=mainnet`, the installer config defaults `ckb.rpc_url` to `https://mainnet.ckb.dev/`. You can still edit `config.yml` later to use your own trusted endpoint.
Do not reuse the same install directory across `testnet` and `mainnet`; keep separate data directories for each network.

### Guided quick-start

If you are working from a local checkout and want a guided install flow:

```bash
# Linux/macOS
./tools/install/quick-start.sh

# Override the default mainnet selection
./tools/install/quick-start.sh ./my-fnn testnet

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

- [Fiber Docs](https://www.fiber.world/docs)
- [GitHub](https://github.com/nervosnetwork/fiber)
- [RPC API](https://github.com/nervosnetwork/fiber/blob/main/crates/fiber-lib/src/rpc/README.md)
