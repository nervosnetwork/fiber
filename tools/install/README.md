# Install Scripts

All installer-related scripts live in `tools/install/`.

## Recommended Entrypoints

### Linux/MacOS

```bash
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install.sh | bash
```

### Unix guided installer

```bash
./tools/install/install.sh
```

If you omit the network argument in guided mode, `install.sh` defaults to `mainnet`.
When it runs via `curl | bash`, the same script uses bootstrap mode and defaults to `testnet`.

### Windows

```powershell
irm https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install.ps1 | iex
```

