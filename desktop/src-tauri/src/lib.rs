use std::fs::OpenOptions;
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

    // Bundled Python is a portable python-build-standalone install, NOT a
    // venv. So python.exe lives at the root, not under Scripts\.
    let python_exe = if cfg!(target_os = "windows") {
        python_dir.join("python.exe")
    } else {
        python_dir.join("bin").join("python")
    };

    log::info!("python_exe = {}", python_exe.display());
    log::info!("app_dir    = {}", app_dir.display());
    log::info!("ffmpeg_dir = {}", ffmpeg_dir.display());

    // Build PATH so backend subprocess can locate ffmpeg and pip-installed
    // scripts (e.g. uvicorn.exe, though we always invoke via -m).
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

    // Redirect stdout+stderr to <app_dir>/backend.log so the user can inspect
    // backend output (including the auto-generated AUTH_PASSWORD warning the
    // first time .env has an empty password) and crash diagnostics.
    let log_path = app_dir.join("backend.log");
    let (stdout_target, stderr_target) = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|f| f.try_clone().map(|f2| (f, f2)))
    {
        Ok((f, f2)) => (Stdio::from(f), Stdio::from(f2)),
        Err(err) => {
            log::warn!(
                "Failed to open backend log file at {}: {err}; falling back to /dev/null",
                log_path.display()
            );
            (Stdio::null(), Stdio::null())
        }
    };

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
        // The bundled portable Python ships its own stdlib under <python>/Lib/.
        // If the end user happens to have PYTHONHOME or PYTHONPATH set in
        // their environment (common for Python developers), the inherited
        // values would override ours and the interpreter would either fail
        // to find its stdlib or pull in the user's site-packages and
        // crash. Strip them.
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONPATH")
        .stdout(stdout_target)
        .stderr(stderr_target);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x08000000 — hide the subprocess console window.
        cmd.creation_flags(0x0800_0000);
    }

    cmd.spawn()
}

/// Outcome of waiting for the backend to come up.
enum BackendStartup {
    /// Health check succeeded — TCP connect + valid HTTP response on /.
    Ready,
    /// The Python child exited (crashed) before the port came up.
    Exited { code: Option<i32> },
    /// The full timeout elapsed without the port responding and without the
    /// child process exiting (e.g. wedged during startup).
    Timeout,
}

/// Poll for the backend HTTP port. While polling, also check whether the
/// spawned child has exited so we can short-circuit the 120s wait when
/// Python crashed during startup (missing dep, port conflict, corrupt
/// install, etc.).
fn wait_for_backend(handle: &tauri::AppHandle) -> BackendStartup {
    let deadline = Instant::now() + Duration::from_secs(HEALTH_CHECK_TIMEOUT_SECS);
    while Instant::now() < deadline {
        if probe_backend() {
            return BackendStartup::Ready;
        }
        // Acquire the backend mutex briefly to call try_wait(); we drop the
        // guard before sleeping so kill_backend() can still acquire it on
        // window close without contention.
        if let Some(state) = handle.try_state::<BackendProcess>() {
            let mut guard = match state.0.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(ref mut child) = guard.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        return BackendStartup::Exited { code: status.code() };
                    }
                    Ok(None) => {} // still running
                    Err(err) => {
                        log::warn!("try_wait on backend failed: {err}");
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(HEALTH_CHECK_POLL_MS));
    }
    BackendStartup::Timeout
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
            let spawn_error: Option<String> = match spawn_backend(&handle) {
                Ok(child) => {
                    log::info!(
                        "Backend spawned (pid={}); waiting for {}:{}",
                        child.id(),
                        BACKEND_HOST,
                        BACKEND_PORT
                    );
                    *state.0.lock().unwrap() = Some(child);
                    None
                }
                Err(err) => {
                    log::error!("Failed to spawn backend: {err}");
                    Some(err.to_string())
                }
            };

            let main_window = handle
                .get_webview_window("main")
                .expect("main window missing");
            let window_for_thread = main_window.clone();
            let handle_for_thread = handle.clone();
            thread::spawn(move || {
                // If the backend process never started, skip the 120s health
                // check entirely and go straight to the error UI. Otherwise
                // poll until either: the port responds, the Python child
                // exits early (crash), or we hit the timeout.
                let outcome = if spawn_error.is_some() {
                    BackendStartup::Timeout // unused; we'll use spawn_error below
                } else {
                    wait_for_backend(&handle_for_thread)
                };
                let ready = spawn_error.is_none() && matches!(outcome, BackendStartup::Ready);
                if ready {
                    // Backend is up. Navigate from the bundled placeholder
                    // (loading spinner) to the live FastAPI app.
                    let target = format!("http://{}:{}/", BACKEND_HOST, BACKEND_PORT);
                    let _ = window_for_thread.eval(&format!(
                        "window.location.replace({});",
                        serde_json::to_string(&target).unwrap_or_else(|_| format!("\"{}\"", target))
                    ));
                } else {
                    let detail = match &spawn_error {
                        Some(msg) => format!(
                            "Backend process could not be started: {msg}. Check backend.log inside the install directory for details."
                        ),
                        None => match outcome {
                            BackendStartup::Exited { code } => format!(
                                "The Python backend exited unexpectedly (code {}) before opening port {}. Check backend.log inside the install directory for the traceback.",
                                code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string()),
                                BACKEND_PORT
                            ),
                            BackendStartup::Timeout => format!(
                                "The Python backend did not respond within {}s. Check backend.log inside the install directory for details, then reopen ArcReel.",
                                HEALTH_CHECK_TIMEOUT_SECS
                            ),
                            BackendStartup::Ready => unreachable!(),
                        },
                    };
                    log::error!("{}", detail);
                    let detail_json = serde_json::to_string(&detail)
                        .unwrap_or_else(|_| "\"Backend failed to start.\"".to_string());
                    // Replace the spinner with an error message so the user
                    // gets actionable info instead of WebView2's
                    // connection-refused page.
                    let _ = window_for_thread.eval(&format!(
                        r#"(function(){{
  var center = document.querySelector('.center');
  if (!center) return;
  center.innerHTML = '';
  var h = document.createElement('div');
  h.style.fontSize = '18px';
  h.style.fontWeight = '600';
  h.style.marginBottom = '8px';
  h.textContent = 'ArcReel backend failed to start';
  var p = document.createElement('div');
  p.style.maxWidth = '480px';
  p.style.textAlign = 'center';
  p.style.fontSize = '13px';
  p.style.lineHeight = '1.5';
  p.style.color = '#94a3b8';
  p.textContent = {};
  center.appendChild(h);
  center.appendChild(p);
}})();"#,
                        detail_json
                    ));
                }
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
