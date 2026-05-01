use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent, WindowEvent};

const BACKEND_HOST: &str = "127.0.0.1";
const BACKEND_PORT: u16 = 1241;
const HEALTH_CHECK_TIMEOUT_SECS: u64 = 120;
const HEALTH_CHECK_POLL_MS: u64 = 250;

struct BackendProcess(Mutex<Option<Child>>);

fn resource_path(handle: &tauri::AppHandle, sub: &str) -> PathBuf {
    handle
        .path()
        .resource_dir()
        .expect("failed to resolve resource_dir")
        .join(sub)
}

fn ensure_env_file(app_dir: &std::path::Path) {
    let env_path = app_dir.join(".env");
    if env_path.exists() {
        return;
    }
    let example = app_dir.join(".env.example");
    if example.exists() {
        if let Err(err) = std::fs::copy(&example, &env_path) {
            log::warn!("failed to seed .env from .env.example: {err}");
        }
    } else if let Err(err) = std::fs::write(
        &env_path,
        b"AUTH_USERNAME=admin\nAUTH_PASSWORD=\n",
    ) {
        log::warn!("failed to write default .env: {err}");
    }
}

fn spawn_backend(handle: &tauri::AppHandle) -> std::io::Result<Child> {
    let python_dir = resource_path(handle, "python");
    let app_dir = resource_path(handle, "app");
    let ffmpeg_dir = resource_path(handle, "ffmpeg");

    // Ensure projects/ exists for the SQLite database file.
    let _ = std::fs::create_dir_all(app_dir.join("projects"));
    ensure_env_file(&app_dir);

    let python_exe = if cfg!(target_os = "windows") {
        python_dir.join("Scripts").join("python.exe")
    } else {
        python_dir.join("bin").join("python")
    };

    log::info!("python_exe = {}", python_exe.display());
    log::info!("app_dir    = {}", app_dir.display());
    log::info!("ffmpeg_dir = {}", ffmpeg_dir.display());

    // Build PATH so backend subprocess can locate ffmpeg and python's Scripts/.
    let path_sep = if cfg!(target_os = "windows") { ";" } else { ":" };
    let scripts_dir = if cfg!(target_os = "windows") {
        python_dir.join("Scripts")
    } else {
        python_dir.join("bin")
    };
    let existing_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!(
        "{}{}{}{}{}",
        scripts_dir.display(),
        path_sep,
        ffmpeg_dir.display(),
        path_sep,
        existing_path
    );

    let mut cmd = Command::new(&python_exe);
    cmd.current_dir(&app_dir)
        .arg("-m")
        .arg("uvicorn")
        .arg("server.app:app")
        .arg("--host")
        .arg(BACKEND_HOST)
        .arg("--port")
        .arg(BACKEND_PORT.to_string())
        .arg("--no-access-log")
        .env("PATH", &new_path)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUNBUFFERED", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x08000000 — hide the subprocess console window.
        cmd.creation_flags(0x0800_0000);
    }

    cmd.spawn()
}

fn wait_for_backend() -> bool {
    let deadline = Instant::now() + Duration::from_secs(HEALTH_CHECK_TIMEOUT_SECS);
    while Instant::now() < deadline {
        if probe_backend() {
            return true;
        }
        thread::sleep(Duration::from_millis(HEALTH_CHECK_POLL_MS));
    }
    false
}

fn probe_backend() -> bool {
    let addr = format!("{}:{}", BACKEND_HOST, BACKEND_PORT);
    let socket_addr = match addr.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let mut stream = match TcpStream::connect_timeout(&socket_addr, Duration::from_millis(500)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    if stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 16];
    matches!(stream.read(&mut buf), Ok(n) if n > 0)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(BackendProcess(Mutex::new(None)))
        .setup(|app| {
            let handle = app.handle().clone();
            let state = handle.state::<BackendProcess>();
            match spawn_backend(&handle) {
                Ok(child) => {
                    log::info!(
                        "Backend spawned (pid={}); waiting for {}:{}",
                        child.id(),
                        BACKEND_HOST,
                        BACKEND_PORT
                    );
                    *state.0.lock().unwrap() = Some(child);
                }
                Err(err) => {
                    log::error!("Failed to spawn backend: {err}");
                }
            }

            let main_window = handle
                .get_webview_window("main")
                .expect("main window missing");
            let window_for_thread = main_window.clone();
            thread::spawn(move || {
                let ready = wait_for_backend();
                if !ready {
                    log::error!(
                        "Backend did not become ready within {}s",
                        HEALTH_CHECK_TIMEOUT_SECS
                    );
                }
                // The window initially loads the bundled placeholder (a loading
                // screen). Once the backend is up, navigate to it. Using eval
                // works regardless of whether navigation succeeds, because the
                // placeholder document is always reachable via tauri:// asset.
                let target = format!(
                    "http://{}:{}/",
                    BACKEND_HOST, BACKEND_PORT
                );
                let _ = window_for_thread.eval(&format!(
                    "window.location.replace({});",
                    serde_json::to_string(&target).unwrap_or_else(|_| format!("\"{}\"", target))
                ));
                let _ = window_for_thread.show();
                let _ = window_for_thread.set_focus();
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::WindowEvent {
                event: WindowEvent::CloseRequested { .. },
                ..
            } = &event
            {
                kill_backend(app_handle);
            }
            if let RunEvent::Exit = event {
                kill_backend(app_handle);
            }
        });
}

fn kill_backend(app_handle: &tauri::AppHandle) {
    let state = app_handle.state::<BackendProcess>();
    // Bind the guard to a named local so its lifetime is clearly the function
    // body (avoids edition-2024 temporary-drop-order issue with `if let` chains).
    let mut guard = match state.0.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(mut child) = guard.take() {
        log::info!("Killing backend pid={}", child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
}
