# ArcReel Desktop

This directory contains the Tauri-based desktop wrapper for ArcReel. It bundles
the Python backend (`server/` + `lib/`), the built React frontend, and a
Windows ffmpeg binary into a single Windows installer (`.exe`) so end users do
not need to install Python, Node, ffmpeg, or `uv` themselves.

## Architecture

```
┌─────────────────────────────────┐
│ ArcReel.exe (Tauri shell)       │   <-- Rust + WebView2
│  ├── spawns python.exe          │
│  ├── waits for :1241            │
│  └── opens window @ http://...  │
└──────────────┬──────────────────┘
               │ subprocess
┌──────────────┴──────────────────┐
│ python.exe -m uvicorn           │
│   server.app:app --port 1241    │
│ (CWD = <install>\app\)          │
└──────────────┬──────────────────┘
               │ /api + SPAStaticFiles
┌──────────────┴──────────────────┐
│ React SPA from frontend/dist    │
└─────────────────────────────────┘
```

The repo's `server/app.py` already mounts `frontend/dist` as a SPA when present,
so when the desktop bundle places the built frontend at
`<install>\app\frontend\dist\`, FastAPI serves both `/api` and the UI from one
port. No backend code is modified — this wrapper is purely additive.

## Bundled resources

The Tauri NSIS installer extracts to `%LOCALAPPDATA%\Programs\ArcReel\`
(per-user install, no admin) with this layout:

| Path                              | Contents                                                     |
| --------------------------------- | ------------------------------------------------------------ |
| `python\Scripts\python.exe`       | uv-managed venv with all backend deps                        |
| `python\Lib\site-packages\…`      | claude-agent-sdk Win wheel ships `_bundled\claude.exe`       |
| `app\server\`, `app\lib\`, `app\alembic\` | Backend source (PROJECT\_ROOT = `<install>\app\`)     |
| `app\frontend\dist\`              | Built frontend (served by SPAStaticFiles on port 1241)       |
| `app\.env.example`                | Default env template (copied to `.env` on first launch)      |
| `app\projects\.arcreel.db`        | SQLite database (created on first launch)                    |
| `ffmpeg\ffmpeg.exe`               | ffmpeg essentials build (added to PATH for the backend)      |

User data (the SQLite DB, `projects/`, `.env`) lives inside the install dir's
`app\` subdirectory. Because the installer uses `currentUser` mode this dir is
fully writable without admin rights.

## Building locally

You need: Windows 10+/11, Python 3.12 on PATH, `uv`, Node 22, pnpm 10, Rust
stable (with `x86_64-pc-windows-msvc` target), and `@tauri-apps/cli` v2.

```powershell
# from repo root
pnpm install -g @tauri-apps/cli@^2.1.0
pnpm --filter ./frontend install --frozen-lockfile
pnpm --filter ./frontend build

pwsh desktop/scripts/prepare-resources.ps1

cd desktop
tauri build --bundles nsis
```

The installer ends up at
`desktop/src-tauri/target/release/bundle/nsis/ArcReel_<version>_x64-setup.exe`.

## CI

`.github/workflows/desktop-build.yml` runs the same steps on a `windows-latest`
runner and uploads the installer as a workflow artifact named
`arcreel-windows-installer`.

## Why a Tauri shell instead of PyInstaller everything?

`claude-agent-sdk` ships an internal `_bundled\claude.exe` per-platform and
many of ArcReel's deps (`alembic`, dynamic SQLAlchemy dialects, Pydantic v2)
are tricky for PyInstaller. Bundling a real `uv` venv plus the source tree
keeps everything that works in dev working in production with zero code
changes — at the cost of a larger installer (~500 MB).
