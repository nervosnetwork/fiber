# Fiber Network Node (FNN) Installer Script for Windows
# Usage: .\install-fnn.ps1 [-InstallDir <path>] [-Network <testnet|mainnet>]
# Example: .\install-fnn.ps1 -InstallDir "C:\my-fnn" -Network testnet

param(
    [string]$InstallDir = ".\my-fnn",
    [string]$Network = "testnet"
)

# Configuration
$FNN_VERSION = "0.6.1"
$CKB_CLI_VERSION = "1.12.0"
$GITHUB_RELEASE_URL = "https://github.com/nervosnetwork/fiber/releases/download/v$FNN_VERSION"
$CKB_CLI_RELEASE_URL = "https://github.com/nervosnetwork/ckb-cli/releases/download/v$CKB_CLI_VERSION"

# Global variable for ckb-cli command
$CKB_CLI_CMD = "ckb-cli"

# Colors for output
function Write-Success($message) {
    Write-Host "✓ $message" -ForegroundColor Green
}

function Write-Warning($message) {
    Write-Host "⚠ $message" -ForegroundColor Yellow
}

function Write-Error($message) {
    Write-Host "✗ $message" -ForegroundColor Red
}

function Write-Info($message) {
    Write-Host "ℹ $message" -ForegroundColor Cyan
}

function Show-Header {
    Write-Host ""
    Write-Host "==========================================" -ForegroundColor Cyan
    Write-Host "    Fiber Network Node (FNN) Installer" -ForegroundColor Cyan
    Write-Host "==========================================" -ForegroundColor Cyan
    Write-Host ""
}

# Platform detection
$PLATFORM = "windows"
$ARCH = if ([System.Environment]::Is64BitOperatingSystem) { "x86_64" } else { "x86" }

# Determine binary names
# Note: FNN Windows release uses .tar.gz format, not .zip
$FNN_BINARY = "fnn_v${FNN_VERSION}-x86_64-windows.tar.gz"
$CKB_CLI_BINARY = "ckb-cli_v${CKB_CLI_VERSION}_x86_64-pc-windows-msvc.zip"

function Test-Command($command) {
    $null = Get-Command $command -ErrorAction SilentlyContinue
    return $?
}

function Install-CkbCli {
    Write-Warning "ckb-cli is required but not found."
    Write-Host ""
    $installCkb = Read-Host "Would you like to automatically download and install ckb-cli? (y/n)"
    
    if ($installCkb -ne "y" -and $installCkb -ne "Y") {
        Write-Info "Please install ckb-cli manually:"
        Write-Host "  https://github.com/nervosnetwork/ckb-cli"
        exit 1
    }
    
    Write-Info "Downloading ckb-cli v$CKB_CLI_VERSION..."
    
    $downloadUrl = "$CKB_CLI_RELEASE_URL/$CKB_CLI_BINARY"
    $tempDir = [System.IO.Path]::GetTempPath() + [System.Guid]::NewGuid().ToString()
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    
    Write-Host "  Downloading from: $downloadUrl"
    
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile "$tempDir\$CKB_CLI_BINARY" -UseBasicParsing
    }
    catch {
        Write-Error "Failed to download ckb-cli: $_"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }
    
    Write-Info "Extracting ckb-cli..."
    Expand-Archive -Path "$tempDir\$CKB_CLI_BINARY" -DestinationPath $tempDir -Force
    
    $ckbCliPath = Get-ChildItem -Path $tempDir -Name "ckb-cli.exe" -Recurse | Select-Object -First 1
    if (-not $ckbCliPath) {
        Write-Error "Could not find ckb-cli.exe in the downloaded archive"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }
    
    $fullCkbCliPath = Join-Path $tempDir $ckbCliPath
    
    # Install to install directory
    Copy-Item $fullCkbCliPath "$InstallDir\ckb-cli.exe"
    $script:CKB_CLI_CMD = "$InstallDir\ckb-cli.exe"
    Write-Success "ckb-cli installed to $InstallDir\ckb-cli.exe"
    
    Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
}

function Check-Prerequisites {
    Write-Info "Checking prerequisites..."
    
    # Check for ckb-cli
    if (-not (Test-Command "ckb-cli")) {
        # Check if ckb-cli exists in install directory
        if (Test-Path "$InstallDir\ckb-cli.exe") {
            Write-Info "Found ckb-cli in install directory"
            $script:CKB_CLI_CMD = "$InstallDir\ckb-cli.exe"
        }
        else {
            Install-CkbCli
        }
    }
    else {
        Write-Success "ckb-cli found"
        $script:CKB_CLI_CMD = (Get-Command "ckb-cli").Source
    }
}

function Download-Binary {
    Write-Info "Downloading FNN binary v$FNN_VERSION for windows-$ARCH..."
    
    $downloadUrl = "$GITHUB_RELEASE_URL/$FNN_BINARY"
    $tempDir = [System.IO.Path]::GetTempPath() + [System.Guid]::NewGuid().ToString()
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    
    Write-Host "  Downloading from: $downloadUrl"
    
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile "$tempDir\$FNN_BINARY" -UseBasicParsing
    }
    catch {
        Write-Error "Failed to download FNN binary: $_"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }
    
    Write-Success "Download completed"
    
    Write-Info "Extracting binary..."
    # Extract .tar.gz file (Windows 10/11 has tar built-in)
    try {
        & tar -xzf "$tempDir\$FNN_BINARY" -C $tempDir 2>&1 | Out-Null
    }
    catch {
        Write-Error "Failed to extract archive. Make sure 'tar' is available (included in Windows 10/11)"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }
    
    $fnnExe = Get-ChildItem -Path $tempDir -Name "fnn.exe" -Recurse | Select-Object -First 1
    if (-not $fnnExe) {
        Write-Error "Could not find fnn.exe in the downloaded archive"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }
    
    $fullFnnPath = Join-Path $tempDir $fnnExe
    Copy-Item $fullFnnPath "$InstallDir\fnn.exe"
    
    Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
    
    Write-Success "Binary installed to $InstallDir\fnn.exe"
}

function Download-Config {
    Write-Info "Downloading configuration files..."
    
    $configUrl = "https://raw.githubusercontent.com/nervosnetwork/fiber/v$FNN_VERSION/config/$Network/config.yml"
    
    try {
        Invoke-WebRequest -Uri $configUrl -OutFile "$InstallDir\config.yml" -UseBasicParsing
    }
    catch {
        Write-Error "Failed to download config: $_"
        exit 1
    }
    
    Write-Success "Configuration downloaded"
}

function Setup-Keys {
    Write-Info "Setting up node keys..."
    
    $ckbDir = "$InstallDir\ckb"
    New-Item -ItemType Directory -Path $ckbDir -Force | Out-Null
    
    Write-Warning "You need a CKB account to run the Fiber node."
    Write-Host ""
    Write-Host "Please choose an option:"
    Write-Host "  1) Create a new CKB account (requires ckb-cli)"
    Write-Host "  2) Use an existing account (you'll need the lock_arg)"
    Write-Host ""
    $choice = Read-Host "Enter your choice (1 or 2)"
    
    switch ($choice) {
        "1" {
            Write-Info "Creating new CKB account..."
            Write-Info "You will be prompted to set a password for your CKB wallet"
            Write-Host ""

            # Run ckb-cli account new directly (not capturing output) so it can interact with user
            & $CKB_CLI_CMD account new

            if ($LASTEXITCODE -ne 0) {
                Write-Error "Failed to create account"
                exit 1
            }

            Write-Host ""
            Write-Info "Account created successfully!"

            # Automatically get the lock_arg from the account list
            Write-Info "Getting account information..."
            Start-Sleep -Seconds 1

            # Get account list and extract the last created account's lock_arg
            $accountList = & $CKB_CLI_CMD account list 2>&1

            if ($LASTEXITCODE -ne 0) {
                Write-Error "Failed to get account list"
                Write-Host $accountList
                exit 1
            }

            # Extract the most recently added account's lock_arg (the last one in the list)
            $lockArgMatch = $accountList | Select-String "lock_arg:" | Select-Object -Last 1
            if ($lockArgMatch) {
                $LOCK_ARG = $lockArgMatch.ToString().Split(":")[1].Trim()
            }

            if (-not $LOCK_ARG) {
                Write-Error "Could not automatically detect lock_arg"
                Write-Info "Please check the account list manually:"
                Write-Host $accountList
                Write-Host ""
                $LOCK_ARG = Read-Host "Enter the lock_arg manually"

                if (-not $LOCK_ARG) {
                    Write-Error "lock_arg is required"
                    exit 1
                }
            }

            Write-Success "Detected lock_arg: $LOCK_ARG"
            # Save to script scope variable for summary
            $script:GlobalLockArg = $LOCK_ARG
        }
        "2" {
            Write-Host ""
            $LOCK_ARG = Read-Host "Enter your lock_arg"
            # Save to script scope variable for summary
            $script:GlobalLockArg = $LOCK_ARG
        }
        default {
            Write-Error "Invalid choice"
            exit 1
        }
    }
    
    Write-Info "Exporting private key..."
    
    try {
        # Export the key
        & $CKB_CLI_CMD account export --lock-arg $LOCK_ARG --extended-privkey-path "$ckbDir\exported-key" 2>&1 | Out-Null
        
        # Extract just the private key (first line)
        $keyContent = Get-Content "$ckbDir\exported-key" -TotalCount 1
        $keyContent | Set-Content "$ckbDir\key"
        
        Write-Success "Private key saved to $ckbDir\key"
        
        # Set permissions (Windows doesn't have chmod, but we can set ACLs)
        $path = "$ckbDir\key"
        $acl = Get-Acl $path
        $acl.SetAccessRuleProtection($true, $false)
        $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
            $env:USERNAME, "Read,Write", "Allow"
        )
        $acl.SetAccessRule($rule)
        Set-Acl $path $acl
        
        $path2 = "$ckbDir\exported-key"
        $acl2 = Get-Acl $path2
        $acl2.SetAccessRuleProtection($true, $false)
        $rule2 = New-Object System.Security.AccessControl.FileSystemAccessRule(
            $env:USERNAME, "Read,Write", "Allow"
        )
        $acl2.SetAccessRule($rule2)
        Set-Acl $path2 $acl2
        
        # Show funding information
        Show-FundingInfo $LOCK_ARG
    }
    catch {
        Write-Error "Failed to export key: $_"
        exit 1
    }
}

function Show-FundingInfo($lockArg) {
    Write-Host ""
    Write-Host "==========================================" -ForegroundColor Yellow
    Write-Host "    IMPORTANT: Fund Your Account" -ForegroundColor Yellow
    Write-Host "==========================================" -ForegroundColor Yellow
    Write-Host ""
    
    Write-Info "Getting your CKB address..."
    try {
        $accountInfo = & $CKB_CLI_CMD account list 2>&1 | Select-String -Context 0,5 $lockArg
        if ($accountInfo) {
            Write-Host ""
            Write-Host "Your account addresses:"
            $accountInfo.Context.PostContext | Select-String "mainnet:|testnet:" | ForEach-Object { Write-Host "  $_" }
            Write-Host ""
        }
    }
    catch {
        # Ignore errors, just show general info
    }
    
    Write-Host "To open payment channels and make transactions, you need CKB tokens."
    Write-Host ""
    
    if ($Network -eq "testnet") {
        Write-Host "For Testnet:"
        Write-Host "  1. Get free testnet CKB from the faucet:"
        Write-Host "     https://faucet.nervos.org/"
        Write-Host ""
        Write-Host "  2. Check the Nervos documentation for other testnet token sources"
        Write-Host ""
        Write-Host "  3. Send testnet CKB to your testnet address (ckt1...)"
    }
    else {
        Write-Host "For Mainnet:"
        Write-Host "  1. Purchase CKB from an exchange (Binance, Coinbase, etc.)"
        Write-Host ""
        Write-Host "  2. Withdraw to your mainnet address (ckb1...)"
        Write-Host ""
        Write-Host "  3. Recommended minimum amount: 1000+ CKB for channel funding"
    }
    
    Write-Host ""
    Write-Host "How to check your balance:"
    Write-Host "  $CKB_CLI_CMD wallet get-capacity --lock-arg $lockArg"
    Write-Host ""
    Write-Warning "Remember: You must have CKB tokens before opening channels!"
    Write-Host ""
}

function Create-StartupScript {
    Write-Info "Creating startup script..."
    
    $scriptContent = @"
# Fiber Network Node Startup Script
# Generated by install-fnn.ps1

`$installDir = `$PSScriptRoot

# Check if password is set
if (-not `$env:FIBER_SECRET_KEY_PASSWORD) {
    `$securePassword = Read-Host "Enter your FIBER_SECRET_KEY_PASSWORD (or set it as environment variable)" -AsSecureString
    `$BSTR = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR(`$securePassword)
    `$env:FIBER_SECRET_KEY_PASSWORD = [System.Runtime.InteropServices.Marshal]::PtrToStringAuto(`$BSTR)
    [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR(`$BSTR)
    Write-Host ""
}

Write-Host "Starting Fiber Network Node..."
Write-Host "  Install directory: `$installDir"
Write-Host "  Config file: `$installDir\config.yml"
Write-Host "  Data directory: `$installDir"
Write-Host ""

Set-Location `$installDir
`$env:RUST_LOG = "info"
.\fnn.exe -c config.yml -d .
"@
    
    $scriptContent | Set-Content "$InstallDir\start-node.ps1" -Encoding UTF8
    Write-Success "Startup script created: $InstallDir\start-node.ps1"
    
    # Also create a batch file for easy double-click execution
    $batchContent = @"
@echo off
powershell -ExecutionPolicy Bypass -File "%~dp0start-node.ps1"
pause
"@
    $batchContent | Set-Content "$InstallDir\start-node.bat" -Encoding ASCII
    Write-Success "Batch script created: $InstallDir\start-node.bat"
    
    # Show password reminder
    Write-Host ""
    Write-Host "==========================================" -ForegroundColor Yellow
    Write-Host "    IMPORTANT: Save Your Password" -ForegroundColor Yellow
    Write-Host "==========================================" -ForegroundColor Yellow
    Write-Host ""
    Write-Host "The FIBER_SECRET_KEY_PASSWORD is used to encrypt your private key."
    Write-Host "You will need this password EVERY TIME you start the node."
    Write-Host ""
    Write-Host "Options to set the password permanently:"
    Write-Host ""
    Write-Host "1. Set environment variable in PowerShell profile:"
    Write-Host '   [Environment]::SetEnvironmentVariable("FIBER_SECRET_KEY_PASSWORD", "your-password", "User")'
    Write-Host ""
    Write-Host "2. Edit the start-node.ps1 script and add the password:"
    Write-Host "   # Replace the Read-Host prompt with:"
    Write-Host '   $env:FIBER_SECRET_KEY_PASSWORD = "your-password"'
    Write-Host ""
    Write-Host "3. Create a wrapper script with the password:"
    Write-Host '   # Create start-with-password.bat:'
    Write-Host '   @echo off'
    Write-Host '   set FIBER_SECRET_KEY_PASSWORD=your-password'
    Write-Host '   start-node.bat'
    Write-Host ""
    Write-Warning "Security Warning:"
    Write-Host "   - Never commit passwords to version control"
    Write-Host "   - Keep your password in a secure password manager"
    Write-Host "   - The password cannot be recovered if lost!"
    Write-Host ""
}

function Create-Readme {
    $readmeContent = @"
# Fiber Network Node

This directory contains your Fiber Network Node installation.

## Files and Directories

- ``fnn.exe`` - The Fiber Network Node binary
- ``config.yml`` - Node configuration file
- ``ckb\key`` - Your private key file (keep this secure!)
- ``start-node.ps1`` - PowerShell script to start the node
- ``start-node.bat`` - Batch script to start the node (double-click to run)
- ``fiber\`` - Node data directory (created on first run)

## Quick Start

### Option 1: Double-click (Easiest)
Double-click ``start-node.bat`` to start the node.

### Option 2: PowerShell
```powershell
.\start-node.ps1
```

### Option 3: Manual
```powershell
`$env:FIBER_SECRET_KEY_PASSWORD = 'your-password'
`$env:RUST_LOG = 'info'
.\fnn.exe -c config.yml -d .
```

The node will start syncing with the $Network.

## Configuration

Edit ``config.yml`` to customize:
- Listening address and port
- RPC settings
- CKB node URL
- UDT whitelist

## Security Notes

- Never share your ``ckb\key`` file
- Keep your FIBER_SECRET_KEY_PASSWORD secure
- The ``ckb\`` directory contains sensitive data - back it up safely

## Upgrading

To upgrade to a new version:
1. Stop the node
2. Backup your data: ``Copy-Item -Recurse fiber\store fiber\store.backup``
3. Download new binary and replace ``fnn.exe``
4. Start the node again

## Documentation

- Fiber Docs: https://docs.fiber.world/
- GitHub: https://github.com/nervosnetwork/fiber
- RPC API: https://github.com/nervosnetwork/fiber/blob/main/src/rpc/README.md
"@
    
    $readmeContent | Set-Content "$InstallDir\README.md" -Encoding UTF8
    Write-Success "README created"
}

function Show-Summary {
    Write-Host ""
    Write-Host "==========================================" -ForegroundColor Green
    Write-Host "    Installation Complete!" -ForegroundColor Green
    Write-Host "==========================================" -ForegroundColor Green
    Write-Host ""
    Write-Host "Your Fiber Network Node is installed at:"
    Write-Host "  $InstallDir"
    Write-Host ""
    Write-Host "To start your node, run:"
    Write-Host "  cd $InstallDir"
    Write-Host "  .\start-node.ps1"
    Write-Host ""
    Write-Host "Or double-click: start-node.bat"
    Write-Host ""
    Write-Host "Important files:"
    Write-Host "  - fnn.exe          : Node binary"
    Write-Host "  - config.yml       : Configuration file"
    Write-Host "  - ckb\key          : Private key (KEEP SECURE!)"
    Write-Host "  - start-node.ps1   : PowerShell startup script"
    Write-Host "  - start-node.bat   : Batch startup script"
    Write-Host ""
    Write-Host "Documentation:"
    Write-Host "  - https://docs.fiber.world/"
    Write-Host "  - https://github.com/nervosnetwork/fiber"
    Write-Host ""
    Write-Warning "Remember to:"
    Write-Host "  1. Keep your private key file (ckb\key) secure"
    Write-Host "  2. Use a strong password for FIBER_SECRET_KEY_PASSWORD"
    Write-Host "  3. Backup your ckb\ directory regularly"
    if ($script:GlobalLockArg) {
        Write-Host "  4. Fund your CKB address:"
        Write-Host "     - Get your address: $CKB_CLI_CMD account list | Select-String '$script:GlobalLockArg' -Context 0,5"
        if ($Network -eq "testnet") {
            Write-Host "     - Testnet faucet: https://faucet.nervos.org/"
        }
    }
    Write-Host ""
    
    # Ask if user wants to start the node now
    Write-Host ""
    $startNow = Read-Host "Would you like to start the node now? (y/n, default: y)"
    if ($startNow -eq "" -or $startNow -eq "y" -or $startNow -eq "Y") {
        Write-Host ""
        Write-Info "Starting Fiber Network Node..."
        Write-Host "  Changing to directory: $InstallDir"
        Write-Host "  Running: .\start-node.ps1"
        Write-Host ""
        Set-Location $InstallDir
        .\start-node.ps1
        # Exit after node stops (user pressed Ctrl-C or node exited)
        exit 0
    } else {
        Write-Host ""
        Write-Info "You can start the node later by running:"
        Write-Host "  cd $InstallDir; .\start-node.ps1"
        Write-Host ""
    }
}

# Main execution
Show-Header

# Validate network
if ($Network -ne "testnet" -and $Network -ne "mainnet") {
    Write-Error "Invalid network: $Network. Must be 'testnet' or 'mainnet'"
    exit 1
}

Write-Info "Installation directory: $InstallDir"
Write-Info "Network: $Network"
Write-Info "Platform: windows-$ARCH"
Write-Host ""

# Check prerequisites
Check-Prerequisites

# Create installation directory
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
Write-Success "Created directory: $InstallDir"

# Download binary and config
Download-Binary
Download-Config

# Setup keys
Write-Host ""
Setup-Keys

# Create startup script
Create-StartupScript

# Create README
Create-Readme

# Print summary
Show-Summary
