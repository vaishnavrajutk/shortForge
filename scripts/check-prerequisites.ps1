$ErrorActionPreference = "Stop"
$checks = @("node", "npm", "docker", "ffmpeg", "ffprobe", "rustc", "cargo")
foreach ($name in $checks) {
  $cmd = Get-Command $name -ErrorAction SilentlyContinue
  if ($cmd) { Write-Host "[OK] $name -> $($cmd.Source)" -ForegroundColor Green }
  else { Write-Host "[MISSING] $name" -ForegroundColor Yellow }
}
Write-Host "`nStart PostgreSQL with: docker compose up -d postgres"
Write-Host "Check it with: docker compose ps"
