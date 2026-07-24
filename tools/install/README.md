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

Mainnet installations do not select a public CKB RPC endpoint automatically.
The guided installer requires you to enter a trusted endpoint. For a
bootstrap or non-interactive mainnet installation, set it explicitly:

```bash
curl -sSfL https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install.sh | NETWORK=mainnet CKB_RPC_URL=https://your-trusted-mainnet-ckb-rpc.example bash
```

### Windows

```powershell
irm https://raw.githubusercontent.com/nervosnetwork/fiber/main/tools/install/install.ps1 | iex
```

## Restarting After Installation

The installer creates startup scripts inside the install directory. After
stopping the node with `Ctrl-C`, restart the same installed node with:

Unix/macOS:

```bash
cd <install-dir>
./start-node.sh
```

Windows PowerShell:

```powershell
cd <install-dir>
.\start-node.ps1
```

Windows CMD:

```cmd
cd <install-dir>
start-node.bat
```

Node data is stored under the install directory. Restarting requires the same
`FIBER_SECRET_KEY_PASSWORD` used when exporting or installing the key. If the
password is wrong, startup can fail with an error such as `Secret key file error: decryption failed: aead::Error`.
