# Prepare bundled resources for the Tauri desktop build (Windows-only).
#
# This script is invoked by the GitHub Actions workflow on a `windows-latest`
# runner. It produces the following layout under `desktop/resources/`:
#
#   python/                  Portable, fully relocatable Python distribution
#                            (downloaded by `uv python install`, which uses
#                            python-build-standalone). All site-packages are
#                            installed directly into this interpreter — no
#                            virtualenv is involved, because Windows venv
#                            launchers hardcode the absolute path of the
#                            base interpreter and would point at the CI
#                            runner's `C:\hostedtoolcache\…` path on the end
#                            user's machine. Installing into the portable
#                            interpreter directly side-steps that problem.
#       python.exe           main interpreter (at the root, not Scripts\)
#       Lib/site-packages/   all backend deps
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
#   <install>\python\python.exe -m uvicorn server.app:app --port 1241
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
# 1. Portable Python interpreter via `uv python install` (python-build-standalone)
# ---------------------------------------------------------------------------
Write-Step "Downloading portable Python 3.12 via 'uv python install'"
# python-build-standalone produces a fully relocatable Python distribution
# (no hardcoded paths in launchers). This is what `uv python install` ships.
# We download it into a scratch dir, find the install root, then copy the
# whole tree to resources/python. The end user's interpreter therefore lives
# entirely under <install>\python\ and does NOT depend on any path on the CI
# runner ever existing on the user's machine.
$UvPythonScratch = Join-Path $env:TEMP "arcreel-uv-python"
if (Test-Path $UvPythonScratch) { Remove-Item -Recurse -Force $UvPythonScratch }
New-Item -ItemType Directory -Force -Path $UvPythonScratch | Out-Null
$env:UV_PYTHON_INSTALL_DIR = $UvPythonScratch

& uv python install 3.12
if ($LASTEXITCODE -ne 0) { throw "uv python install failed (exit $LASTEXITCODE)" }

# `uv python install` lays out cpython-3.12.x-windows-x86_64-...\python.exe
# inside its install dir. Find the directory that contains python.exe at root.
$PythonInstall = Get-ChildItem -Path $UvPythonScratch -Directory -Recurse |
    Where-Object { Test-Path (Join-Path $_.FullName "python.exe") } |
    Select-Object -First 1
if (-not $PythonInstall) {
    throw "Could not locate python.exe inside $UvPythonScratch after 'uv python install'"
}
Write-Information "Found portable Python at $($PythonInstall.FullName)"

Write-Step "Copying portable Python -> resources/python"
Copy-Item -Recurse -Force $PythonInstall.FullName $PyDir
$PythonExe = Join-Path $PyDir "python.exe"
if (-not (Test-Path $PythonExe)) {
    throw "Expected $PythonExe to exist after copy"
}

Write-Step "Exporting locked dependencies via uv export"
# Use uv.lock as the single source of truth for pinned versions, but skip
# the project itself (we're going to copy server/lib/alembic into app/
# separately) and skip dev deps (tests, ruff, etc.).
$ReqFile = Join-Path $env:TEMP "arcreel-requirements.txt"
Push-Location $RepoRoot
try {
    & uv export --frozen --no-dev --no-emit-project --format requirements.txt --output-file $ReqFile
    if ($LASTEXITCODE -ne 0) { throw "uv export failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

Write-Step "Installing backend deps directly into portable Python"
# Use the bundled Python's own pip; this populates <python>\Lib\site-packages
# with all runtime deps. No venv launcher, no hardcoded paths.
& $PythonExe -m pip install --no-cache-dir --no-warn-script-location --upgrade pip
if ($LASTEXITCODE -ne 0) { throw "pip self-upgrade failed (exit $LASTEXITCODE)" }
& $PythonExe -m pip install --no-cache-dir --no-warn-script-location -r $ReqFile
if ($LASTEXITCODE -ne 0) { throw "pip install -r requirements failed (exit $LASTEXITCODE)" }

# Drop unused junk from site-packages to shrink the bundle.
$JunkPaths = @(
    (Join-Path $PyDir "Lib\site-packages\setuptools"),
    (Join-Path $PyDir "Lib\site-packages\wheel"),
    (Join-Path $PyDir "Lib\site-packages\_distutils_hack"),
    (Join-Path $PyDir "Lib\site-packages\pkg_resources")
)
foreach ($p in $JunkPaths) {
    if (Test-Path $p) { Remove-Item -Recurse -Force $p -ErrorAction SilentlyContinue }
}
Get-ChildItem -Path $PyDir -Recurse -Force -Include "__pycache__", "*.pyc" -ErrorAction SilentlyContinue | `
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

# Smoke test: verify the bundled interpreter can import a few critical deps
# (catches platform-mismatch issues immediately instead of at runtime on the
# user's machine).
Write-Step "Smoke-testing bundled Python"
& $PythonExe -c "import sys; print(sys.executable); import fastapi, uvicorn, sqlalchemy, alembic; print('imports OK')"
if ($LASTEXITCODE -ne 0) { throw "Bundled Python smoke test failed (exit $LASTEXITCODE)" }

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
