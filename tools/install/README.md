# Install Scripts

All installer-related scripts live in `tools/install/`.

## Recommended Entrypoints

### Linux/MacOS

```bash
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | bash
```

### Unix guided installer

```bash
./tools/install/quick-start.sh
```

If you omit the network argument, `quick-start.sh` defaults to `mainnet`.

### Windows

```powershell
irm https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-fnn.ps1 | iex
```


