# Fiber Network Node (FNN) Installer Script for Windows
# Usage: .\tools\install\install-fnn.ps1 [-InstallDir <path>] [-Network <testnet|mainnet>]
# Example: .\tools\install\install-fnn.ps1 -InstallDir "C:\my-fnn" -Network testnet

param(
    [string]$InstallDir = ".\my-fnn",
    [string]$Network = "testnet"
)

# Allow the same env overrides as the shell installers when params are omitted.
if ($InstallDir -eq ".\my-fnn" -and $env:INSTALL_DIR) {
    $InstallDir = $env:INSTALL_DIR
}
if ($Network -eq "testnet" -and $env:NETWORK) {
    $Network = $env:NETWORK
}

# Configuration
$FNN_VERSION = if ($env:FNN_VERSION) { $env:FNN_VERSION } else { "0.8.0" }
$CKB_CLI_VERSION = if ($env:CKB_CLI_VERSION) { $env:CKB_CLI_VERSION } else { "1.12.0" }
$GITHUB_RELEASE_URL = "https://github.com/nervosnetwork/fiber/releases/download/v$FNN_VERSION"
$CKB_CLI_RELEASE_URL = "https://github.com/nervosnetwork/ckb-cli/releases/download/v$CKB_CLI_VERSION"
$DEFAULT_MAINNET_CKB_RPC_URL = "https://mainnet.ckb.dev/"
$NETWORK_MARKER_FILE_NAME = ".fiber-network"
$MAINNET_GENESIS_HASH = "0x92b197aa1fba0f63633922c61c92375c9c074a93e85963554f5499fe1450d0e5"
$TESTNET_GENESIS_HASH = "0x10639e0895502b5688a6be8cf69460d76541bfa4821629d86d62ba0aae3f9606"
$PUBLIC_NODE_ANNOUNCED_ADDR_PLACEHOLDER = "/ip4/YOUR-FIBER-NODE-PUBLIC-IP/tcp/8228"
$PUBLIC_NODE_NAME_PLACEHOLDER = "my-fiber-node"
$script:StartupBlockerMessage = $null

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

function Get-ConfigValueInSection {
    param(
        [string]$ConfigPath,
        [string]$SectionName,
        [string]$KeyName
    )

    if (-not (Test-Path $ConfigPath)) {
        return $null
    }

    $inSection = $false
    $keyPattern = [regex]::Escape($KeyName)
    foreach ($line in [System.IO.File]::ReadAllLines($ConfigPath)) {
        if ($line -match '^[^\s#][^:]*:\s*$') {
            $inSection = ($line -eq "$SectionName:")
            continue
        }

        if ($inSection -and $line -match "^\s*$keyPattern:\s*(.*)$") {
            $value = $Matches[1] -replace '\s+#.*$', ''
            $value = $value.Trim()
            return $value.Trim('"')
        }
    }

    return $null
}

function Set-ConfigValueInSection {
    param(
        [string]$ConfigPath,
        [string]$SectionName,
        [string]$KeyName,
        [string]$KeyValue
    )

    if (-not (Test-Path $ConfigPath)) {
        return $false
    }

    $inSection = $false
    $updated = $false
    $keyPattern = [regex]::Escape($KeyName)
    $newLines = New-Object System.Collections.Generic.List[string]

    foreach ($line in [System.IO.File]::ReadAllLines($ConfigPath)) {
        if ($line -match '^[^\s#][^:]*:\s*$') {
            $inSection = ($line -eq "$SectionName:")
            $newLines.Add($line)
            continue
        }

        if ($inSection -and $line -match "^\s*$keyPattern:\s*") {
            $newLines.Add("  $KeyName: `"$KeyValue`"")
            $updated = $true
            continue
        }

        $newLines.Add($line)
    }

    if (-not $updated) {
        return $false
    }

    Set-Content -Path $ConfigPath -Value $newLines -Encoding UTF8
    return $true
}

function Set-RawConfigValueInSection {
    param(
        [string]$ConfigPath,
        [string]$SectionName,
        [string]$KeyName,
        [string]$KeyValue
    )

    if (-not (Test-Path $ConfigPath)) {
        return $false
    }

    $inSection = $false
    $updated = $false
    $keyPattern = [regex]::Escape($KeyName)
    $newLines = New-Object System.Collections.Generic.List[string]

    foreach ($line in [System.IO.File]::ReadAllLines($ConfigPath)) {
        if ($line -match '^[^\s#][^:]*:\s*$') {
            if ($inSection -and -not $updated) {
                $newLines.Add("  $KeyName: $KeyValue")
                $updated = $true
            }
            $inSection = ($line -eq "$SectionName:")
            $newLines.Add($line)
            continue
        }

        if ($inSection -and $line -match "^\s*$keyPattern:\s*") {
            $newLines.Add("  $KeyName: $KeyValue")
            $updated = $true
            continue
        }

        $newLines.Add($line)
    }

    if ($inSection -and -not $updated) {
        $newLines.Add("  $KeyName: $KeyValue")
    }

    Set-Content -Path $ConfigPath -Value $newLines -Encoding UTF8
    return $true
}

function Remove-ConfigValueInSection {
    param(
        [string]$ConfigPath,
        [string]$SectionName,
        [string]$KeyName
    )

    if (-not (Test-Path $ConfigPath)) {
        return $false
    }

    $inSection = $false
    $keyPattern = [regex]::Escape($KeyName)
    $newLines = New-Object System.Collections.Generic.List[string]

    foreach ($line in [System.IO.File]::ReadAllLines($ConfigPath)) {
        if ($line -match '^[^\s#][^:]*:\s*$') {
            $inSection = ($line -eq "$SectionName:")
            $newLines.Add($line)
            continue
        }

        if ($inSection -and $line -match "^\s*$keyPattern:\s*") {
            continue
        }

        $newLines.Add($line)
    }

    Set-Content -Path $ConfigPath -Value $newLines -Encoding UTF8
    return $true
}

function Set-AnnouncedAddrsConfig {
    param(
        [string]$ConfigPath,
        [string]$AnnouncedAddr
    )

    if (-not (Test-Path $ConfigPath)) {
        return $false
    }

    $inFiber = $false
    $replacing = $false
    $found = $false
    $newLines = New-Object System.Collections.Generic.List[string]

    foreach ($line in [System.IO.File]::ReadAllLines($ConfigPath)) {
        if ($line -match '^[^\s#][^:]*:\s*$') {
            $inFiber = ($line -eq "fiber:")
            $newLines.Add($line)
            continue
        }

        if ($inFiber -and $line -match '^\s*announced_addrs:') {
            if ([string]::IsNullOrWhiteSpace($AnnouncedAddr)) {
                $newLines.Add("  announced_addrs: []")
            }
            else {
                $newLines.Add("  announced_addrs:")
                $newLines.Add("    - `"$AnnouncedAddr`"")
            }
            $found = $true
            $replacing = $true
            continue
        }

        if ($replacing) {
            if ($line -match '^  [^\s#][^:]*:\s*') {
                $replacing = $false
                $newLines.Add($line)
            }
            continue
        }

        $newLines.Add($line)
    }

    if (-not $found) {
        return $false
    }

    Set-Content -Path $ConfigPath -Value $newLines -Encoding UTF8
    return $true
}

function Escape-YamlDoubleQuotedValue {
    param([string]$Value)

    return $Value.Replace('\', '\\').Replace('"', '\"')
}

function Get-ExistingInstallNetwork {
    $markerPath = Join-Path $InstallDir $NETWORK_MARKER_FILE_NAME
    $configPath = Join-Path $InstallDir "config.yml"

    if (Test-Path $markerPath) {
        return (Get-Content $markerPath -TotalCount 1).Trim()
    }

    return Get-ConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "chain"
}

function Assert-InstallDirMatchesNetwork {
    $existingNetwork = Get-ExistingInstallNetwork
    $dataDir = Join-Path $InstallDir "fiber"
    $hasData = $false

    if (Test-Path $dataDir) {
        $hasData = [bool](Get-ChildItem -Path $dataDir -Force -ErrorAction SilentlyContinue | Select-Object -First 1)
    }

    if ($existingNetwork -and $existingNetwork -ne $Network -and $hasData) {
        Write-Error "Install directory $InstallDir already contains $existingNetwork data."
        Write-Host "  Reusing it for $Network can mix network graph state and cause chain hash mismatch warnings."
        Write-Host "  Use a different install directory, or remove $InstallDir\fiber before switching networks."
        exit 1
    }
}

function Write-InstallNetworkMarker {
    Set-Content -Path (Join-Path $InstallDir $NETWORK_MARKER_FILE_NAME) -Value $Network -Encoding ASCII
}

function Apply-NetworkConfigDefaults {
    $configPath = Join-Path $InstallDir "config.yml"
    $rpcUrlOverride = $env:CKB_RPC_URL

    if (-not $rpcUrlOverride -and $Network -eq "mainnet") {
        $rpcUrlOverride = $DEFAULT_MAINNET_CKB_RPC_URL
    }

    if ($rpcUrlOverride) {
        if (-not (Set-ConfigValueInSection -ConfigPath $configPath -SectionName "ckb" -KeyName "rpc_url" -KeyValue $rpcUrlOverride)) {
            Write-Error "Failed to update ckb.rpc_url in $configPath"
            exit 1
        }
    }
}

function Get-CkbRpcUrlFromConfig {
    return Get-ConfigValueInSection -ConfigPath (Join-Path $InstallDir "config.yml") -SectionName "ckb" -KeyName "rpc_url"
}

function Configure-CkbRpcUrl {
    if ($Network -ne "mainnet") {
        return
    }

    if ($env:CKB_RPC_URL) {
        Write-Success "Configured CKB RPC URL: $env:CKB_RPC_URL"
        return
    }

    $currentRpcUrl = Get-CkbRpcUrlFromConfig
    Write-Host ""
    Write-Warning "Mainnet requires a reachable CKB RPC endpoint."
    Write-Host "  Press Enter to use the default public RPC, or provide your own trusted endpoint."
    Write-Host "  Current ckb.rpc_url: $currentRpcUrl"
    $desiredRpcUrl = Read-Host "Enter the CKB RPC URL to use (press Enter to keep the current value)"
    if ([string]::IsNullOrWhiteSpace($desiredRpcUrl)) {
        $desiredRpcUrl = $currentRpcUrl
    }

    if (-not (Set-ConfigValueInSection -ConfigPath (Join-Path $InstallDir "config.yml") -SectionName "ckb" -KeyName "rpc_url" -KeyValue $desiredRpcUrl)) {
        Write-Error "Failed to update ckb.rpc_url in $InstallDir\config.yml"
        exit 1
    }

    Write-Success "Configured CKB RPC URL: $desiredRpcUrl"
}

function Configure-MainnetPublicNode {
    if ($Network -ne "mainnet") {
        return
    }

    $configPath = Join-Path $InstallDir "config.yml"
    Write-Host ""
    $publicChoice = Read-Host "Should this mainnet node be a public Fiber node? (y/n, default: n)"
    if ([string]::IsNullOrWhiteSpace($publicChoice)) {
        $publicChoice = "n"
    }

    $isPublic = switch -Regex ($publicChoice) {
        '^(?i:y|yes)$' { $true; break }
        '^(?i:n|no)$' { $false; break }
        default {
            Write-Error "Invalid choice: $publicChoice"
            exit 1
        }
    }

    if ($isPublic) {
        Write-Host ""
        Write-Info "Configure the public address announced to the Fiber network."
        Write-Host "  Placeholder: $PUBLIC_NODE_ANNOUNCED_ADDR_PLACEHOLDER"
        do {
            $announcedAddr = Read-Host "Enter announced_addrs"
            if ([string]::IsNullOrWhiteSpace($announcedAddr)) {
                Write-Warning "announced_addrs cannot be empty for a public mainnet node."
            }
        } while ([string]::IsNullOrWhiteSpace($announcedAddr))

        Write-Host ""
        Write-Info "Configure the node name announced to the Fiber network."
        Write-Host "  Placeholder: $PUBLIC_NODE_NAME_PLACEHOLDER"
        do {
            $announcedNodeName = Read-Host "Enter announced_node_name"
            if ([string]::IsNullOrWhiteSpace($announcedNodeName)) {
                Write-Warning "announced_node_name cannot be empty for a public mainnet node."
            }
        } while ([string]::IsNullOrWhiteSpace($announcedNodeName))

        if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "auto_announce_node" -KeyValue "true")) {
            Write-Error "Failed to update fiber.auto_announce_node in $configPath"
            exit 1
        }
        if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announce_listening_addr" -KeyValue "true")) {
            Write-Error "Failed to update fiber.announce_listening_addr in $configPath"
            exit 1
        }
        if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announced_node_name" -KeyValue "`"$(Escape-YamlDoubleQuotedValue $announcedNodeName)`"")) {
            Write-Error "Failed to update fiber.announced_node_name in $configPath"
            exit 1
        }
        if (-not (Set-AnnouncedAddrsConfig -ConfigPath $configPath -AnnouncedAddr $announcedAddr)) {
            Write-Error "Failed to update fiber.announced_addrs in $configPath"
            exit 1
        }

        Write-Success "Configured this mainnet node as a public Fiber node"
        return
    }

    if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "auto_announce_node" -KeyValue "false")) {
        Write-Error "Failed to update fiber.auto_announce_node in $configPath"
        exit 1
    }
    if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announce_listening_addr" -KeyValue "false")) {
        Write-Error "Failed to update fiber.announce_listening_addr in $configPath"
        exit 1
    }
    if (-not (Remove-ConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announced_node_name")) {
        Write-Error "Failed to update fiber.announced_node_name in $configPath"
        exit 1
    }
    if (-not (Set-AnnouncedAddrsConfig -ConfigPath $configPath -AnnouncedAddr "")) {
        Write-Error "Failed to update fiber.announced_addrs in $configPath"
        exit 1
    }

    Write-Success "Configured this mainnet node as a non-public Fiber node"
}

function Check-CkbRpcPreflight {
    $rpcUrl = Get-CkbRpcUrlFromConfig
    if (-not $rpcUrl) {
        $script:StartupBlockerMessage = "Could not read ckb.rpc_url from $InstallDir\config.yml."
        return $false
    }

    $expectedGenesisHash = switch ($Network) {
        "mainnet" { $MAINNET_GENESIS_HASH }
        "testnet" { $TESTNET_GENESIS_HASH }
        default {
            $script:StartupBlockerMessage = "Unsupported network: $Network"
            return $false
        }
    }

    try {
        $response = Invoke-RestMethod -Uri $rpcUrl -Method Post -ContentType "application/json" -Body '{"id":2,"jsonrpc":"2.0","method":"get_block_hash","params":["0x0"]}'
    }
    catch {
        $script:StartupBlockerMessage = "Cannot reach the configured CKB RPC at $rpcUrl."
        return $false
    }

    $actualGenesisHash = $response.result
    if (-not $actualGenesisHash) {
        $script:StartupBlockerMessage = "The CKB RPC at $rpcUrl did not return a usable genesis hash."
        return $false
    }

    if ($actualGenesisHash -ne $expectedGenesisHash) {
        $script:StartupBlockerMessage = "The configured CKB RPC at $rpcUrl does not appear to be a $Network node."
        return $false
    }

    return $true
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
    Write-Info "Downloading Fiber release bundle v$FNN_VERSION for windows-$ARCH..."
    
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
    
    Write-Info "Extracting release bundle..."
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
    $fnnCliExe = Get-ChildItem -Path $tempDir -Name "fnn-cli.exe" -Recurse | Select-Object -First 1
    $fnnMigrateExe = Get-ChildItem -Path $tempDir -Name "fnn-migrate.exe" -Recurse | Select-Object -First 1
    $configDir = Get-ChildItem -Path $tempDir -Directory -Recurse | Where-Object { $_.Name -eq "config" } | Select-Object -First 1

    if (-not $fnnExe -or -not $fnnCliExe -or -not $fnnMigrateExe -or -not $configDir) {
        Write-Error "The downloaded release bundle is missing required files."
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }

    Copy-Item (Join-Path $tempDir $fnnExe) "$InstallDir\fnn.exe"
    Copy-Item (Join-Path $tempDir $fnnCliExe) "$InstallDir\fnn-cli.exe"
    Copy-Item (Join-Path $tempDir $fnnMigrateExe) "$InstallDir\fnn-migrate.exe"
    Remove-Item -Recurse -Force "$InstallDir\config" -ErrorAction SilentlyContinue
    Copy-Item $configDir.FullName "$InstallDir\config" -Recurse

    Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue

    Write-Success "Release bundle installed to $InstallDir"
}

function Download-Config {
    Write-Info "Preparing configuration files..."

    $configRoot = Join-Path $InstallDir "config"
    $missingTemplates = $false
    New-Item -ItemType Directory -Path $configRoot -Force | Out-Null

    foreach ($templateNetwork in @("testnet", "mainnet")) {
        $templateDir = Join-Path $configRoot $templateNetwork
        $templateConfig = Join-Path $templateDir "config.yml"

        if (-not (Test-Path $templateConfig)) {
            $missingTemplates = $true
            New-Item -ItemType Directory -Path $templateDir -Force | Out-Null
            try {
                Invoke-WebRequest -Uri "https://raw.githubusercontent.com/nervosnetwork/fiber/v$FNN_VERSION/config/$templateNetwork/config.yml" -OutFile $templateConfig -UseBasicParsing
            }
            catch {
                Write-Error "Failed to download config for ${templateNetwork}: $_"
                exit 1
            }
        }
    }

    $bundledConfig = "$InstallDir\config\$Network\config.yml"
    if (-not (Test-Path $bundledConfig)) {
        Write-Error "Could not prepare the selected config template: $bundledConfig"
        exit 1
    }

    Copy-Item $bundledConfig "$InstallDir\config.yml" -Force
    
    Apply-NetworkConfigDefaults
    Write-InstallNetworkMarker
    if ($missingTemplates) {
        Write-Success "Configuration prepared with downloaded network templates"
    }
    else {
        Write-Success "Configuration prepared from bundled network templates"
    }
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
        
        # Extract only the private key and remove the exported extended key material.
        $keyContent = Get-Content "$ckbDir\exported-key" -TotalCount 1
        $keyContent | Set-Content "$ckbDir\key"
        Remove-Item "$ckbDir\exported-key" -Force
        
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
`$networkMarkerFile = Join-Path `$installDir "$NETWORK_MARKER_FILE_NAME"
`$configNetwork = `$null
`$inFiberSection = `$false
foreach (`$line in [System.IO.File]::ReadAllLines((Join-Path `$installDir "config.yml"))) {
    if (`$line -match '^[^\s#][^:]*:\s*$') {
        `$inFiberSection = (`$line -eq "fiber:")
        continue
    }

    if (`$inFiberSection -and `$line -match '^\s*chain:\s*(.*)$') {
        `$configNetwork = (`$Matches[1] -replace '\s+#.*$', '').Trim().Trim('"')
        break
    }
}

if (Test-Path `$networkMarkerFile) {
    `$installedNetwork = (Get-Content `$networkMarkerFile -TotalCount 1).Trim()
    if (`$configNetwork -and `$installedNetwork -ne `$configNetwork) {
        Write-Host "Error: This install directory is marked for `$installedNetwork, but config.yml is set to `$configNetwork." -ForegroundColor Red
        Write-Host "Use a separate directory for each network, or remove `$installDir\fiber before switching networks."
        exit 1
    }
}
elseif (`$configNetwork) {
    Set-Content -Path `$networkMarkerFile -Value `$configNetwork -Encoding ASCII
}

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
- ``fnn-cli.exe`` - Fiber command-line utility
- ``fnn-migrate.exe`` - Database migration utility
- ``config\`` - Bundled network configuration templates for both testnet and mainnet
- ``config.yml`` - Active node configuration file for the selected network
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
3. Download the new release bundle and replace ``fnn.exe``, ``fnn-cli.exe``, ``fnn-migrate.exe``, and ``config\``
4. Start the node again

## Documentation

- Fiber Docs: https://docs.fiber.world/
- GitHub: https://github.com/nervosnetwork/fiber
- RPC API: https://github.com/nervosnetwork/fiber/blob/main/crates/fiber-lib/src/rpc/README.md
"@
    
    $readmeContent | Set-Content "$InstallDir\README.md" -Encoding UTF8
    Write-Success "README created"
}

function Show-Summary {
    $canStartNow = $true

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
    Write-Host "  - fnn-cli.exe      : CLI utility"
    Write-Host "  - fnn-migrate.exe  : Database migration utility"
    Write-Host "  - config\          : Bundled config templates"
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

    if (-not (Check-CkbRpcPreflight)) {
        $canStartNow = $false
        Write-Warning "Skipping automatic startup because the configured CKB RPC is not ready."
        Write-Host "  $script:StartupBlockerMessage"
        Write-Host "  Update ckb.rpc_url in $InstallDir\config.yml and try again."
        Write-Host ""
    }

    if ($canStartNow) {
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
        }
    }

    Write-Host ""
    Write-Info "You can start the node later by running:"
    Write-Host "  cd $InstallDir; .\start-node.ps1"
    if (-not $canStartNow) {
        Write-Host "  # First update ckb.rpc_url in config.yml to a reachable $Network CKB RPC"
    }
    Write-Host ""
}

# Main execution
Show-Header

# Validate network
if ($Network -ne "testnet" -and $Network -ne "mainnet") {
    Write-Error "Invalid network: $Network. Must be 'testnet' or 'mainnet'"
    exit 1
}

Assert-InstallDirMatchesNetwork

Write-Info "Installation directory: $InstallDir"
Write-Info "Network: $Network"
Write-Info "Platform: windows-$ARCH"
Write-Host ""

# Create installation directory
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
Write-Success "Created directory: $InstallDir"

# Check prerequisites
Check-Prerequisites

# Download binary and config
Download-Binary
Download-Config
Configure-CkbRpcUrl
Configure-MainnetPublicNode

# Setup keys
Write-Host ""
Setup-Keys

# Create startup script
Create-StartupScript

# Create README
Create-Readme

# Print summary
Show-Summary
