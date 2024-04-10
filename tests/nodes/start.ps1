Set-Location -Path $PSScriptRoot

$jobs = @()
try {
  $jobs += Start-Job -ScriptBlock { cargo run -- -d 1 }
  $jobs += Start-Job -ScriptBlock { cargo run -- -d 2 }
  $jobs += Start-Job -ScriptBlock { cargo run -- -d 3 }

  # Wait for jobs to complete
  $jobs | Format-Table 
  $jobs | Receive-Job -Wait -Force -WriteEvents
} finally {
  Write-Warning "Ctrl+C detected. Stopping jobs..."
  $jobs | Stop-Job
  $jobs | Remove-Job
}
