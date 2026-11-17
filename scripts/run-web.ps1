$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$env:SHORTFORGE_BROWSER_MODE = "1"

$backend = $null

Push-Location $repoRoot
try {
    docker compose up -d postgres
    if ($LASTEXITCODE -ne 0) { throw "PostgreSQL could not start in Docker Compose." }

    docker compose up -d --build web
    if ($LASTEXITCODE -ne 0) { throw "The ShortForge browser container could not be built or started." }

    # An optimised build is several times faster at face tracking and does not restart when files change.
    & npm.cmd run build
    if ($LASTEXITCODE -ne 0) { throw "The ShortForge frontend could not be built." }
    & cargo build --release --manifest-path (Join-Path $repoRoot "src-tauri/Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "The ShortForge local service could not be built." }
    $binary = Join-Path $repoRoot "src-tauri/target/release/shortforge.exe"
    $backend = Start-Process -FilePath $binary -WorkingDirectory $repoRoot -WindowStyle Hidden -PassThru
    $deadline = (Get-Date).AddSeconds(120)
    $ready = $false

    while ((Get-Date) -lt $deadline -and -not $ready) {
        try {
            $health = Invoke-RestMethod -Uri "http://127.0.0.1:1421/api/health" -TimeoutSec 2
            $ready = $health.ok -eq $true
        } catch {
            Start-Sleep -Milliseconds 500
        }
        if ($backend.HasExited) {
            throw "ShortForge could not start. Check PostgreSQL and the local FFmpeg setup."
        }
    }

    if (-not $ready) {
        throw "The local ShortForge browser service did not become ready within 120 seconds."
    }

    Start-Process "http://127.0.0.1:1420"
    Write-Host "ShortForge web is running from its Docker container. Keep this command open while using it; Ctrl+C stops the web container."
    Wait-Process -Id $backend.Id
} finally {
    if ($backend -and -not $backend.HasExited) {
        & taskkill.exe /PID $backend.Id /T /F | Out-Null
    }
    docker compose stop web
    Pop-Location
}
