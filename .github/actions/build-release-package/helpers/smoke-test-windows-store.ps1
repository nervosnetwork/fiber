param(
    [string]$Executable = "target\release\fnn.exe",
    [string]$Name = "fnn-smoke"
)

$smokeDir = Join-Path $env:RUNNER_TEMP $Name
Remove-Item $smokeDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path (Join-Path $smokeDir "fiber\store") -Force | Out-Null
$env:FIBER_SECRET_KEY_PASSWORD = "ci-smoke-test"

$output = & $Executable --check-validate -c config\testnet\config.yml -d $smokeDir 2>&1
$exitCode = $LASTEXITCODE
$output | Write-Output

if ($exitCode -eq 0) {
    exit 0
}

if ($exitCode -eq 1 -and $output -match "db validate failed") {
    exit 0
}

throw "Windows store smoke test failed with exit code $exitCode"
