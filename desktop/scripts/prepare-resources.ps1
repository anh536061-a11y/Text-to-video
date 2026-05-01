# Prepare bundled resources for the Tauri desktop build (Windows-only).
#
# This script is invoked by the GitHub Actions workflow on a `windows-latest`
# runner. It produces the following layout under `desktop/resources/`:
#
#   python/                  uv-managed virtualenv with all backend deps
#   app/                     copy of the backend source tree (server, lib, alembic, …)
#       server/              FastAPI app
#       lib/                 core library (PROJECT_ROOT = app/)
#       alembic/             migrations
#       alembic.ini
#       agent_runtime_profile/
#       public/
#       .env.example
#       frontend/dist/       built frontend (served by FastAPI's SPAStaticFiles)
#   ffmpeg/ffmpeg.exe        statically-linked Windows ffmpeg
#
# The Tauri shell at runtime runs:
#   <install>\python\Scripts\python.exe -m uvicorn server.app:app --port 1241
# with cwd = <install>\app\, which makes PROJECT_ROOT = <install>\app\.
#
# Usage:
#   pwsh desktop/scripts/prepare-resources.ps1

$ErrorActionPreference = "Stop"
$InformationPreference = "Continue"

function Write-Step([string]$Msg) {
    Write-Information ""
    Write-Information "==> $Msg"
}

# Resolve repo root (this script lives at <root>/desktop/scripts/).
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$DesktopDir = Join-Path $RepoRoot "desktop"
$ResDir = Join-Path $DesktopDir "resources"
$AppDir = Join-Path $ResDir "app"
$PyDir = Join-Path $ResDir "python"
$FfDir = Join-Path $ResDir "ffmpeg"

Write-Step "Cleaning previous bundled resources"
if (Test-Path $ResDir) {
    Remove-Item -Recurse -Force $ResDir
}
New-Item -ItemType Directory -Force -Path $ResDir, $AppDir, $FfDir | Out-Null

# ---------------------------------------------------------------------------
# 1. Backend Python venv via uv
# ---------------------------------------------------------------------------
Write-Step "Creating backend virtualenv via uv sync (no-dev, no-install-project)"
# We deliberately do NOT install the arcreel project itself into the venv
# (--no-install-project). The backend source (server/, lib/, alembic/) is
# copied into `app/` separately and Python finds it via `cwd` when we run
# `python -m uvicorn server.app:app` with cwd = app/. This avoids the .pth
# absolute-path problem that arises when copying an editable venv to a
# different machine / install path.
Push-Location $RepoRoot
try {
    if (Test-Path ".venv") { Remove-Item -Recurse -Force ".venv" }
    & uv sync --no-dev --no-install-project --frozen
    if ($LASTEXITCODE -ne 0) { throw "uv sync failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

Write-Step "Copying .venv -> resources/python (this may take a while; ~400 MB)"
Copy-Item -Recurse -Force (Join-Path $RepoRoot ".venv") $PyDir

# Drop unused junk from the venv to shrink the bundle.
$VenvJunkPaths = @(
    (Join-Path $PyDir "Lib\site-packages\pip"),
    (Join-Path $PyDir "Lib\site-packages\setuptools"),
    (Join-Path $PyDir "Lib\site-packages\wheel"),
    (Join-Path $PyDir "Lib\site-packages\_distutils_hack"),
    (Join-Path $PyDir "Lib\site-packages\pkg_resources")
)
foreach ($p in $VenvJunkPaths) {
    if (Test-Path $p) { Remove-Item -Recurse -Force $p -ErrorAction SilentlyContinue }
}
Get-ChildItem -Path $PyDir -Recurse -Force -Include "__pycache__", "*.pyc" -ErrorAction SilentlyContinue | `
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

# ---------------------------------------------------------------------------
# 2. Backend source tree
# ---------------------------------------------------------------------------
Write-Step "Copying backend source -> resources/app"
$AppCopyTargets = @(
    "server",
    "lib",
    "alembic",
    "agent_runtime_profile",
    "public"
)
foreach ($name in $AppCopyTargets) {
    $src = Join-Path $RepoRoot $name
    if (Test-Path $src) {
        Copy-Item -Recurse -Force $src $AppDir
    }
}
foreach ($file in @("alembic.ini", ".env.example", "pyproject.toml")) {
    $src = Join-Path $RepoRoot $file
    if (Test-Path $src) {
        Copy-Item -Force $src $AppDir
    }
}

# Strip __pycache__ inside copied source.
Get-ChildItem -Path $AppDir -Recurse -Force -Include "__pycache__", "*.pyc" -ErrorAction SilentlyContinue | `
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

# ---------------------------------------------------------------------------
# 3. Frontend (already built by the workflow before invoking this script).
# ---------------------------------------------------------------------------
Write-Step "Copying built frontend -> resources/app/frontend/dist"
$FrontendDist = Join-Path $RepoRoot "frontend\dist"
if (-not (Test-Path $FrontendDist)) {
    throw "frontend/dist not found at $FrontendDist — run 'pnpm --filter ./frontend build' first."
}
$AppFrontend = Join-Path $AppDir "frontend\dist"
New-Item -ItemType Directory -Force -Path (Split-Path $AppFrontend) | Out-Null
Copy-Item -Recurse -Force $FrontendDist (Split-Path $AppFrontend)

# ---------------------------------------------------------------------------
# 4. ffmpeg (statically-linked Windows build from gyan.dev)
# ---------------------------------------------------------------------------
Write-Step "Downloading ffmpeg (essentials build, ~80 MB)"
$FfmpegZip = Join-Path $env:TEMP "ffmpeg-release-essentials.zip"
$FfmpegUrl = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip"
Invoke-WebRequest -Uri $FfmpegUrl -OutFile $FfmpegZip -UseBasicParsing
$FfmpegExtract = Join-Path $env:TEMP "ffmpeg-extract"
if (Test-Path $FfmpegExtract) { Remove-Item -Recurse -Force $FfmpegExtract }
Expand-Archive -Path $FfmpegZip -DestinationPath $FfmpegExtract
$FfmpegExe = Get-ChildItem -Path $FfmpegExtract -Filter "ffmpeg.exe" -Recurse | Select-Object -First 1
if (-not $FfmpegExe) {
    throw "ffmpeg.exe not found inside extracted archive at $FfmpegExtract"
}
Copy-Item -Force $FfmpegExe.FullName (Join-Path $FfDir "ffmpeg.exe")
$FfprobeExe = Get-ChildItem -Path $FfmpegExtract -Filter "ffprobe.exe" -Recurse | Select-Object -First 1
if ($FfprobeExe) {
    Copy-Item -Force $FfprobeExe.FullName (Join-Path $FfDir "ffprobe.exe")
}

# ---------------------------------------------------------------------------
# 5. Summary
# ---------------------------------------------------------------------------
Write-Step "Resource bundle ready"
Get-ChildItem $ResDir | Select-Object Name, Mode | Format-Table | Out-String | Write-Information
$total = (Get-ChildItem -Recurse -Force $ResDir | Measure-Object -Property Length -Sum).Sum
Write-Information ("Total size: {0:N0} MB" -f ($total / 1MB))
