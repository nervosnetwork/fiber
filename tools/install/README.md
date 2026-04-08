# Install Scripts

All installer-related scripts live in `tools/install/`.

## Files

- `install-common.sh`: Shared shell helpers used by the Unix installers.
- `install-curl.sh`: Non-interactive Unix installer for `curl | bash`, extracting the full release bundle.
- `quick-start.sh`: The primary guided Unix installer with wallet/key setup.
- `install-fnn.ps1`: Guided Windows installer.


## Recommended Entrypoints

### Unix one-liner

```bash
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-curl.sh | bash
```

### Unix guided installer

```bash
./tools/install/quick-start.sh
```

### Unix guided installer with an existing local binary

```bash
./tools/install/quick-start.sh --local-binary ./fnn
```

### Windows guided installer

```powershell
.\tools\install\install-fnn.ps1
```

### Windows remote installer

```powershell
irm https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install-fnn.ps1 | iex
```

If you need to test an unpublished public branch or fork, replace `main` in the GitHub Raw URL, or set `INSTALL_REPO` and `INSTALL_REF` for the Unix one-liner.
The examples below assume these installer files have already been published on the upstream `main` branch.

