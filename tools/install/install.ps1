# Fiber Network Node (FNN) Installer Script for Windows
# Usage: .\tools\install\install.ps1 [-InstallDir <path>] [-Network <testnet|mainnet>]
# Example: .\tools\install\install.ps1 -InstallDir "C:\my-fnn" -Network mainnet

param(
    [string]$InstallDir = ".\my-fnn",
    [string]$Network = "mainnet"
)

# Allow the same env overrides as the shell installers when params are omitted.
if ($InstallDir -eq ".\my-fnn" -and $env:INSTALL_DIR) {
    $InstallDir = $env:INSTALL_DIR
}
if ($Network -eq "mainnet" -and $env:NETWORK) {
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

# Keep console output ASCII-only so Windows PowerShell 5.1 can read this file
# reliably even when the checkout is UTF-8 without BOM.
function Write-Success($message) {
    Write-Host "[OK] $message" -ForegroundColor Green
}

function Write-FnnWarning($message) {
    Write-Host "[WARN] $message" -ForegroundColor Yellow
}

function Write-FnnError($message) {
    Write-Host "[ERROR] $message" -ForegroundColor Red
}

function Write-Info($message) {
    Write-Host "[INFO] $message" -ForegroundColor Cyan
}

function Restart-InstallerFromTempFileIfNeeded {
    $invocationPath = $null
    if ($MyInvocation -and $MyInvocation.MyCommand) {
        $invocationPath = $MyInvocation.MyCommand.Path
    }

    if (-not [string]::IsNullOrWhiteSpace($PSCommandPath) -or -not [string]::IsNullOrWhiteSpace($invocationPath)) {
        return $false
    }

    $scriptBlock = $null
    if ($MyInvocation -and $MyInvocation.MyCommand) {
        $scriptBlock = $MyInvocation.MyCommand.ScriptBlock
    }
    if (-not $scriptBlock) {
        Write-FnnWarning "Could not re-launch the installer from a temporary file. Interactive password prompts may look degraded in 'irm | iex' mode."
        return $false
    }

    $tempScriptPath = Join-Path ([System.IO.Path]::GetTempPath()) ("fnn-install-{0}.ps1" -f [System.Guid]::NewGuid().ToString())
    $powershellExe = Join-Path $PSHOME "powershell.exe"
    if (-not (Test-Path $powershellExe)) {
        $powershellExe = "powershell"
    }

    try {
        [System.IO.File]::WriteAllText($tempScriptPath, $scriptBlock.ToString(), [System.Text.UTF8Encoding]::new($false))
        Write-Info "Re-launching the installer from a temporary .ps1 file so ckb-cli can use normal password prompts."
        & $powershellExe -NoProfile -ExecutionPolicy Bypass -File $tempScriptPath -InstallDir $InstallDir -Network $Network
        $global:LASTEXITCODE = $LASTEXITCODE
    }
    finally {
        Remove-Item -LiteralPath $tempScriptPath -Force -ErrorAction SilentlyContinue
    }

    return $true
}

if (Restart-InstallerFromTempFileIfNeeded) {
    return
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

function Enable-Tls12ForDownloads {
    try {
        $currentProtocols = [System.Net.ServicePointManager]::SecurityProtocol
        $tls12 = [System.Net.SecurityProtocolType]::Tls12
        $tls13 = 0
        if ([enum]::GetNames([System.Net.SecurityProtocolType]) -contains "Tls13") {
            $tls13 = [System.Net.SecurityProtocolType]::Tls13
        }

        [System.Net.ServicePointManager]::SecurityProtocol = $currentProtocols -bor $tls12 -bor $tls13
    }
    catch {
        Write-FnnWarning "Failed to explicitly enable TLS 1.2+. Downloads may fail on older Windows builds."
    }
}

function New-TemporaryDirectory {
    $tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    return $tempDir
}

function Find-FirstFileInDirectory {
    param(
        [string]$SearchPath,
        [string]$FileName
    )

    return Get-ChildItem -Path $SearchPath -Filter $FileName -File -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
}

function Find-FirstDirectoryInDirectory {
    param(
        [string]$SearchPath,
        [string]$DirectoryName
    )

    return Get-ChildItem -Path $SearchPath -Directory -Recurse -ErrorAction SilentlyContinue | Where-Object { $_.Name -eq $DirectoryName } | Select-Object -First 1
}

function Expand-TarGzArchive {
    param(
        [string]$ArchivePath,
        [string]$DestinationPath
    )

    $tarCmd = Get-Command tar -ErrorAction SilentlyContinue
    if (-not $tarCmd) {
        Write-FnnError "Failed to extract archive. Make sure 'tar' is available (included in Windows 10/11)."
        exit 1
    }

    & $tarCmd.Source -xzf $ArchivePath -C $DestinationPath 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-FnnError "Failed to extract archive: $ArchivePath"
        exit 1
    }
}

function Set-KeyFilePermissions {
    param([string]$Path)

    try {
        $currentIdentity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
        $acl = Get-Acl $Path
        $acl.SetAccessRuleProtection($true, $false)
        $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
            $currentIdentity.Name,
            [System.Security.AccessControl.FileSystemRights]::Read -bor [System.Security.AccessControl.FileSystemRights]::Write,
            [System.Security.AccessControl.AccessControlType]::Allow
        )
        $acl.SetAccessRule($rule)
        Set-Acl $Path $acl
    }
    catch {
        Write-FnnWarning "Saved the private key, but could not tighten file permissions automatically."
        Write-Host "  Review the ACL for $Path manually if this machine is shared."
    }
}

# Determine binary names
# Note: FNN Windows release uses .tar.gz format, not .zip
$FNN_BINARY = "fnn_v${FNN_VERSION}-x86_64-windows.tar.gz"
$CKB_CLI_BINARY = "ckb-cli_v${CKB_CLI_VERSION}_x86_64-pc-windows-msvc.zip"

function Test-Command($command) {
    return $null -ne (Get-Command $command -ErrorAction SilentlyContinue)
}

function Get-ExistingCkbCliPath {
    if (Test-Command "ckb-cli") {
        return (Get-Command "ckb-cli").Source
    }

    $localCkbCli = Join-Path $InstallDir "ckb-cli.exe"
    if (Test-Path $localCkbCli) {
        return $localCkbCli
    }

    return $null
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
            $inSection = ($line -eq "${SectionName}:")
            continue
        }

        if ($inSection -and $line -match "^\s*${keyPattern}:\s*(.*)$") {
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
            $inSection = ($line -eq "${SectionName}:")
            $newLines.Add($line)
            continue
        }

        if ($inSection -and $line -match "^\s*${keyPattern}:\s*") {
            $newLines.Add("  ${KeyName}: `"$KeyValue`"")
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
                $newLines.Add("  ${KeyName}: $KeyValue")
                $updated = $true
            }
            $inSection = ($line -eq "${SectionName}:")
            $newLines.Add($line)
            continue
        }

        if ($inSection -and $line -match "^\s*${keyPattern}:\s*") {
            $newLines.Add("  ${KeyName}: $KeyValue")
            $updated = $true
            continue
        }

        $newLines.Add($line)
    }

    if ($inSection -and -not $updated) {
        $newLines.Add("  ${KeyName}: $KeyValue")
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
            $inSection = ($line -eq "${SectionName}:")
            $newLines.Add($line)
            continue
        }

        if ($inSection -and $line -match "^\s*${keyPattern}:\s*") {
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

function Normalize-InstallDir {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $Path
    }

    if ($Path -eq "~") {
        return $HOME
    }

    if ($Path.StartsWith("~/") -or $Path.StartsWith('~\')) {
        $relativePath = $Path.Substring(2).TrimStart('\', '/')
        $Path = Join-Path $HOME $relativePath
    }

    try {
        return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
    }
    catch {
        return [System.IO.Path]::GetFullPath($Path)
    }
}

function Test-InteractiveStdin {
    try {
        return -not [Console]::IsInputRedirected
    }
    catch {
        return $true
    }
}

function Test-InstallPathHasExistingContents {
    param([string]$Path)

    if (-not (Test-Path $Path)) {
        return $false
    }

    if (-not (Test-Path $Path -PathType Container)) {
        return $true
    }

    return [bool](Get-ChildItem -Path $Path -Force -ErrorAction SilentlyContinue | Select-Object -First 1)
}

function Get-InstallBackupDir {
    param([string]$Path)

    $timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $candidate = "${Path}.backup-$timestamp"
    $suffix = 1

    while (Test-Path $candidate) {
        $candidate = "${Path}.backup-$timestamp-$suffix"
        $suffix += 1
    }

    return $candidate
}

function Backup-ExistingInstallPath {
    param(
        [string]$ExistingPath,
        [string]$BackupPath
    )

    Move-Item -LiteralPath $ExistingPath -Destination $BackupPath
    Write-Success "Backed up existing install path to $BackupPath"
}

function Prepare-InstallDir {
    $resolvedInstallDir = Normalize-InstallDir $InstallDir

    while (Test-InstallPathHasExistingContents -Path $resolvedInstallDir) {
        $backupDir = Get-InstallBackupDir -Path $resolvedInstallDir

        Write-FnnWarning "Install directory already exists and is not empty: $resolvedInstallDir"

        if (-not (Test-InteractiveStdin)) {
            Backup-ExistingInstallPath -ExistingPath $resolvedInstallDir -BackupPath $backupDir
            break
        }

        Write-Host "  Press Enter to back it up to:"
        Write-Host "    $backupDir"
        Write-Host "  Or type a different install directory path."
        $userInput = Read-Host "Install directory choice (default: back up current directory)"

        if ([string]::IsNullOrWhiteSpace($userInput)) {
            Backup-ExistingInstallPath -ExistingPath $resolvedInstallDir -BackupPath $backupDir
            break
        }

        $resolvedInstallDir = Normalize-InstallDir $userInput
        Write-Info "Using installation directory: $resolvedInstallDir"
    }

    return $resolvedInstallDir
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
        Write-FnnError "Install directory $InstallDir already contains $existingNetwork data."
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
            Write-FnnError "Failed to update ckb.rpc_url in $configPath"
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
    Write-FnnWarning "Mainnet requires a reachable CKB RPC endpoint."
    Write-Host "  Press Enter to use the default public RPC, or provide your own trusted endpoint."
    Write-Host "  Current ckb.rpc_url: $currentRpcUrl"
    $desiredRpcUrl = Read-Host "Enter the CKB RPC URL to use (press Enter to keep the current value)"
    if ([string]::IsNullOrWhiteSpace($desiredRpcUrl)) {
        $desiredRpcUrl = $currentRpcUrl
    }

    if (-not (Set-ConfigValueInSection -ConfigPath (Join-Path $InstallDir "config.yml") -SectionName "ckb" -KeyName "rpc_url" -KeyValue $desiredRpcUrl)) {
        Write-FnnError "Failed to update ckb.rpc_url in $InstallDir\config.yml"
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
            Write-FnnError "Invalid choice: $publicChoice"
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
                Write-FnnWarning "announced_addrs cannot be empty for a public mainnet node."
            }
        } while ([string]::IsNullOrWhiteSpace($announcedAddr))

        Write-Host ""
        Write-Info "Configure the node name announced to the Fiber network."
        Write-Host "  Placeholder: $PUBLIC_NODE_NAME_PLACEHOLDER"
        Write-Host "  Press Enter to skip announced_node_name."
        $announcedNodeName = Read-Host "Enter announced_node_name (optional)"

        if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "auto_announce_node" -KeyValue "true")) {
            Write-FnnError "Failed to update fiber.auto_announce_node in $configPath"
            exit 1
        }
        if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announce_listening_addr" -KeyValue "true")) {
            Write-FnnError "Failed to update fiber.announce_listening_addr in $configPath"
            exit 1
        }
        if ([string]::IsNullOrWhiteSpace($announcedNodeName)) {
            if (-not (Remove-ConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announced_node_name")) {
                Write-FnnError "Failed to update fiber.announced_node_name in $configPath"
                exit 1
            }
        } elseif (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announced_node_name" -KeyValue "`"$(Escape-YamlDoubleQuotedValue $announcedNodeName)`"")) {
            Write-FnnError "Failed to update fiber.announced_node_name in $configPath"
            exit 1
        }
        if (-not (Set-AnnouncedAddrsConfig -ConfigPath $configPath -AnnouncedAddr $announcedAddr)) {
            Write-FnnError "Failed to update fiber.announced_addrs in $configPath"
            exit 1
        }

        Write-Success "Configured this mainnet node as a public Fiber node"
        return
    }

    if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "auto_announce_node" -KeyValue "false")) {
        Write-FnnError "Failed to update fiber.auto_announce_node in $configPath"
        exit 1
    }
    if (-not (Set-RawConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announce_listening_addr" -KeyValue "false")) {
        Write-FnnError "Failed to update fiber.announce_listening_addr in $configPath"
        exit 1
    }
    if (-not (Remove-ConfigValueInSection -ConfigPath $configPath -SectionName "fiber" -KeyName "announced_node_name")) {
        Write-FnnError "Failed to update fiber.announced_node_name in $configPath"
        exit 1
    }
    if (-not (Set-AnnouncedAddrsConfig -ConfigPath $configPath -AnnouncedAddr "")) {
        Write-FnnError "Failed to update fiber.announced_addrs in $configPath"
        exit 1
    }

    Write-Success "Configured this mainnet node as a non-public Fiber node"
}

function Check-CkbRpcPreflight {
    $rpcUrl = Get-CkbRpcUrlFromConfig
    if (-not $rpcUrl) {
        return [pscustomobject]@{
            Ok = $false
            Message = "Could not read ckb.rpc_url from $InstallDir\config.yml."
        }
    }

    $expectedGenesisHash = switch ($Network) {
        "mainnet" { $MAINNET_GENESIS_HASH }
        "testnet" { $TESTNET_GENESIS_HASH }
        default {
            return [pscustomobject]@{
                Ok = $false
                Message = "Unsupported network: $Network"
            }
        }
    }

    try {
        $response = Invoke-RestMethod -Uri $rpcUrl -Method Post -ContentType "application/json" -Body '{"id":2,"jsonrpc":"2.0","method":"get_block_hash","params":["0x0"]}' -ErrorAction Stop
    }
    catch {
        return [pscustomobject]@{
            Ok = $false
            Message = "Cannot reach the configured CKB RPC at $rpcUrl."
        }
    }

    $actualGenesisHash = $response.result
    if (-not $actualGenesisHash) {
        return [pscustomobject]@{
            Ok = $false
            Message = "The CKB RPC at $rpcUrl did not return a usable genesis hash."
        }
    }

    if ($actualGenesisHash -ne $expectedGenesisHash) {
        return [pscustomobject]@{
            Ok = $false
            Message = "The configured CKB RPC at $rpcUrl does not appear to be a $Network node."
        }
    }

    return [pscustomobject]@{
        Ok = $true
        Message = $null
    }
}

function Install-CkbCli {
    Write-FnnWarning "ckb-cli is required but not found."
    Write-Host ""
    $installCkb = Read-Host "Would you like to automatically download and install ckb-cli? (y/n)"

    if ($installCkb -ne "y" -and $installCkb -ne "Y") {
        Write-Info "Please install ckb-cli manually:"
        Write-Host "  https://github.com/nervosnetwork/ckb-cli"
        exit 1
    }

    Write-Info "Downloading ckb-cli v$CKB_CLI_VERSION..."

    $downloadUrl = "$CKB_CLI_RELEASE_URL/$CKB_CLI_BINARY"
    $tempDir = New-TemporaryDirectory

    Write-Host "  Downloading from: $downloadUrl"

    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile "$tempDir\$CKB_CLI_BINARY" -UseBasicParsing -ErrorAction Stop
    }
    catch {
        Write-FnnError "Failed to download ckb-cli: $_"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }

    Write-Info "Extracting ckb-cli..."
    try {
        Expand-Archive -Path "$tempDir\$CKB_CLI_BINARY" -DestinationPath $tempDir -Force -ErrorAction Stop
    }
    catch {
        Write-FnnError "Failed to extract ckb-cli: $_"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }

    $ckbCliPath = Find-FirstFileInDirectory -SearchPath $tempDir -FileName "ckb-cli.exe"
    if (-not $ckbCliPath) {
        Write-FnnError "Could not find ckb-cli.exe in the downloaded archive"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }

    # Install to install directory
    Copy-Item $ckbCliPath.FullName "$InstallDir\ckb-cli.exe"
    $installedCkbCliPath = "$InstallDir\ckb-cli.exe"
    Write-Success "ckb-cli installed to $installedCkbCliPath"

    Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
    return $installedCkbCliPath
}

function Check-Prerequisites {
    Write-Info "Checking prerequisites..."

    $existingCkbCli = Get-ExistingCkbCliPath
    if ($existingCkbCli) {
        Write-Success "ckb-cli found at $existingCkbCli"
        return $existingCkbCli
    }

    return Install-CkbCli
}

function Download-Binary {
    Write-Info "Downloading Fiber release bundle v$FNN_VERSION for windows-$ARCH..."

    $downloadUrl = "$GITHUB_RELEASE_URL/$FNN_BINARY"
    $tempDir = New-TemporaryDirectory

    Write-Host "  Downloading from: $downloadUrl"

    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile "$tempDir\$FNN_BINARY" -UseBasicParsing -ErrorAction Stop
    }
    catch {
        Write-FnnError "Failed to download FNN binary: $_"
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }

    Write-Success "Download completed"

    Write-Info "Extracting release bundle..."
    Expand-TarGzArchive -ArchivePath "$tempDir\$FNN_BINARY" -DestinationPath $tempDir

    $fnnExe = Find-FirstFileInDirectory -SearchPath $tempDir -FileName "fnn.exe"
    $fnnCliExe = Find-FirstFileInDirectory -SearchPath $tempDir -FileName "fnn-cli.exe"
    $fnnMigrateExe = Find-FirstFileInDirectory -SearchPath $tempDir -FileName "fnn-migrate.exe"
    $configDir = Find-FirstDirectoryInDirectory -SearchPath $tempDir -DirectoryName "config"

    if (-not $fnnExe -or -not $fnnCliExe -or -not $fnnMigrateExe -or -not $configDir) {
        Write-FnnError "The downloaded release bundle is missing required files."
        Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
        exit 1
    }

    Copy-Item $fnnExe.FullName "$InstallDir\fnn.exe"
    Copy-Item $fnnCliExe.FullName "$InstallDir\fnn-cli.exe"
    Copy-Item $fnnMigrateExe.FullName "$InstallDir\fnn-migrate.exe"
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
                Invoke-WebRequest -Uri "https://raw.githubusercontent.com/nervosnetwork/fiber/v$FNN_VERSION/config/$templateNetwork/config.yml" -OutFile $templateConfig -UseBasicParsing -ErrorAction Stop
            }
            catch {
                Write-FnnError "Failed to download config for ${templateNetwork}: $_"
                exit 1
            }
        }
    }

    $bundledConfig = "$InstallDir\config\$Network\config.yml"
    if (-not (Test-Path $bundledConfig)) {
        Write-FnnError "Could not prepare the selected config template: $bundledConfig"
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
    $createdNewAccount = $false

    Write-FnnWarning "You need a CKB account to run the Fiber node."
    Write-Host ""
    Write-Host "Please choose an option:"
    Write-Host "  1) Create a new CKB account (requires ckb-cli)"
    Write-Host "  2) Use an existing account (you'll need the lock_arg)"
    Write-Host ""
    $choice = Read-Host "Enter your choice (1 or 2)"

    switch ($choice) {
        "1" {
            Write-Info "Creating new CKB account..."
            Write-Info "ckb-cli will ask for your CKB wallet password three times during setup:"
            Write-Host "  1) Enter a new wallet password"
            Write-Host "  2) Enter the same wallet password again to confirm it"
            Write-Host "  3) Enter the same wallet password once more to export the key for FNN"
            Write-Host ""

            # Run ckb-cli account new directly (not capturing output) so it can interact with user
            & $CKB_CLI_CMD account new

            if ($LASTEXITCODE -ne 0) {
                Write-FnnError "Failed to create account"
                exit 1
            }

            $createdNewAccount = $true

            Write-Host ""
            Write-Info "Account created successfully!"

            # Automatically get the lock_arg from the account list
            Write-Info "Getting account information..."
            Start-Sleep -Seconds 1

            # Get account list and extract the last created account's lock_arg
            $accountList = & $CKB_CLI_CMD account list 2>&1

            if ($LASTEXITCODE -ne 0) {
                Write-FnnError "Failed to get account list"
                Write-Host $accountList
                exit 1
            }

            # Extract the most recently added account's lock_arg (the last one in the list)
            $lockArgMatch = $accountList | Select-String "lock_arg:" | Select-Object -Last 1
            if ($lockArgMatch) {
                $LOCK_ARG = $lockArgMatch.ToString().Split(":")[1].Trim()
            }

            if ([string]::IsNullOrWhiteSpace($LOCK_ARG)) {
                Write-FnnError "Could not automatically detect lock_arg"
                Write-Info "Please check the account list manually:"
                Write-Host $accountList
                Write-Host ""
                $LOCK_ARG = Read-Host "Enter the lock_arg manually"

                if ([string]::IsNullOrWhiteSpace($LOCK_ARG)) {
                    Write-FnnError "lock_arg is required"
                    exit 1
                }
            }

            Write-Success "Detected lock_arg: $LOCK_ARG"
        }
        "2" {
            Write-Host ""
            $LOCK_ARG = Read-Host "Enter your lock_arg"
        }
        default {
            Write-FnnError "Invalid choice"
            exit 1
        }
    }

    if ([string]::IsNullOrWhiteSpace($LOCK_ARG)) {
        Write-FnnError "lock_arg is required"
        exit 1
    }

    Write-Info "Exporting private key..."
    if ($createdNewAccount) {
        Write-Info "When ckb-cli prompts next, enter the same CKB wallet password you just created."
    }
    else {
        Write-Info "When ckb-cli prompts next, enter the password for the selected CKB wallet."
    }

    try {
        # Run ckb-cli account export directly so it can prompt for the wallet password.
        & $CKB_CLI_CMD account export --lock-arg $LOCK_ARG --extended-privkey-path "$ckbDir\exported-key"
        if ($LASTEXITCODE -ne 0) {
            Write-FnnError "Failed to export key"
            exit 1
        }

        # Extract only the private key and remove the exported extended key material.
        $keyContent = Get-Content "$ckbDir\exported-key" -TotalCount 1
        $keyContent | Set-Content "$ckbDir\key"
        Remove-Item "$ckbDir\exported-key" -Force

        Write-Success "Private key saved to $ckbDir\key"

        Set-KeyFilePermissions -Path "$ckbDir\key"

        # Show funding information
        Show-FundingInfo $LOCK_ARG
    }
    catch {
        Write-FnnError "Failed to export key: $_"
        exit 1
    }

    return $LOCK_ARG
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
    Write-FnnWarning "Remember: You must have CKB tokens before opening channels!"
    Write-Host ""
}

function Create-StartupScript {
    Write-Info "Creating startup script..."

    $scriptContent = @"
# Fiber Network Node Startup Script
# Generated by install.ps1

`$installDir = `$PSScriptRoot
`$networkMarkerFile = Join-Path `$installDir "$NETWORK_MARKER_FILE_NAME"
`$configNetwork = `$null
`$inFiberSection = `$false
if (-not (Test-Path (Join-Path `$installDir "config.yml"))) {
    Write-Host "Error: Could not find config.yml in `$installDir." -ForegroundColor Red
    exit 1
}

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
    `$env:FIBER_SECRET_KEY_PASSWORD = [System.Runtime.InteropServices.Marshal]::PtrToStringBSTR(`$BSTR)
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
    Write-FnnWarning "Security Warning:"
    Write-Host "   - Never commit passwords to version control"
    Write-Host "   - Keep your password in a secure password manager"
    Write-Host "   - The password cannot be recovered if lost!"
    Write-Host ""
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
    Write-Host "  - https://www.fiber.world/docs/quick-start/run-a-node"
    Write-Host ""
    Write-FnnWarning "Remember to:"
    Write-Host "  1. Keep your private key file (ckb\key) secure"
    Write-Host "  2. Use a strong password for FIBER_SECRET_KEY_PASSWORD"
    Write-Host "  3. Backup your ckb\ directory regularly"
    if ($GlobalLockArg) {
        Write-Host "  4. Fund your CKB address:"
        Write-Host "     - Get your address: $CKB_CLI_CMD account list | Select-String '$GlobalLockArg' -Context 0,5"
        if ($Network -eq "testnet") {
            Write-Host "     - Testnet faucet: https://faucet.nervos.org/"
        }
    }
    Write-Host ""

    $rpcPreflight = Check-CkbRpcPreflight
    if (-not $rpcPreflight.Ok) {
        $canStartNow = $false
        Write-FnnWarning "Skipping automatic startup because the configured CKB RPC is not ready."
        Write-Host "  $($rpcPreflight.Message)"
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
Enable-Tls12ForDownloads

if ($ARCH -ne "x86_64") {
    Write-FnnError "Only 64-bit Windows is supported by the published FNN release bundle."
    exit 1
}

# Validate network
if ($Network -ne "testnet" -and $Network -ne "mainnet") {
    Write-FnnError "Invalid network: $Network. Must be 'testnet' or 'mainnet'"
    exit 1
}

$InstallDir = Prepare-InstallDir
Assert-InstallDirMatchesNetwork

Write-Info "Installation directory: $InstallDir"
Write-Info "Network: $Network"
Write-Info "Platform: windows-$ARCH"
Write-Host ""

# Create installation directory
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
Write-Success "Created directory: $InstallDir"

# Check prerequisites
$CKB_CLI_CMD = Check-Prerequisites

# Download binary and config
Download-Binary
Download-Config
Configure-CkbRpcUrl
Configure-MainnetPublicNode

# Setup keys
Write-Host ""
$GlobalLockArg = Setup-Keys

# Create startup script
Create-StartupScript

# Print summary
Show-Summary
