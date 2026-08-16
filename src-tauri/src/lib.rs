use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, LogicalSize, Manager, PhysicalPosition, PhysicalSize, Position, Size, State,
    WebviewWindow, Window, WindowEvent,
};

const BALL_WINDOW_LABEL: &str = "ball";
const MAIN_WINDOW_LABEL: &str = "main";
const SETTINGS_WINDOW_LABEL: &str = "settings";

const MENU_SHOW_MAIN: &str = "show_main_panel";
const MENU_TOGGLE_BALL: &str = "toggle_usage_ball";
const MENU_SHOW_SETTINGS: &str = "show_settings";
const MENU_EXIT: &str = "exit_app";
const WINDOW_STATE_FILE: &str = "window-positions.json";
const RIGHT_SNAP_DISTANCE_PX: i32 = 24;
const BALL_MIN_SIZE: u32 = 88;
const BALL_MAX_SIZE: u32 = 160;
const BALL_WINDOW_EXTRA_HEIGHT: u32 = 28;
const RATE_LIMIT_CACHE_TTL: Duration = Duration::from_secs(180);
const WINDOW_POSITION_SAVE_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Default)]
struct TrayState {
    ball_toggle_item: Option<CheckMenuItem<tauri::Wry>>,
}

struct WindowPositionSaveState {
    sender: mpsc::Sender<()>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredWindowPosition {
    x: i32,
    y: i32,
    snapped_to_right: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowSize {
    width: i32,
    height: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkArea {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowPositionStore {
    ball: Option<StoredWindowPosition>,
    main: Option<StoredWindowPosition>,
    settings: Option<StoredWindowPosition>,
}

impl WindowPositionStore {
    fn set(&mut self, label: &str, position: StoredWindowPosition) -> bool {
        let slot = match label {
            BALL_WINDOW_LABEL => &mut self.ball,
            MAIN_WINDOW_LABEL => &mut self.main,
            SETTINGS_WINDOW_LABEL => &mut self.settings,
            _ => return false,
        };

        if *slot == Some(position) {
            return false;
        }

        *slot = Some(position);
        true
    }
}

fn right_edge_x(work_area: WorkArea, size: WindowSize) -> i32 {
    work_area.x + (work_area.width - size.width).max(0)
}

fn bottom_edge_y(work_area: WorkArea, size: WindowSize) -> i32 {
    work_area.y + (work_area.height - size.height).max(0)
}

fn center_y(work_area: WorkArea, size: WindowSize) -> i32 {
    work_area.y + (work_area.height - size.height).max(0) / 2
}

fn clamp_window_position(
    position: StoredWindowPosition,
    work_area: WorkArea,
    size: WindowSize,
) -> StoredWindowPosition {
    StoredWindowPosition {
        x: position.x.clamp(work_area.x, right_edge_x(work_area, size)),
        y: position
            .y
            .clamp(work_area.y, bottom_edge_y(work_area, size)),
        snapped_to_right: position.snapped_to_right,
    }
}

fn snap_ball_to_right(
    position: StoredWindowPosition,
    work_area: WorkArea,
    size: WindowSize,
) -> StoredWindowPosition {
    let clamped = clamp_window_position(position, work_area, size);
    StoredWindowPosition {
        x: right_edge_x(work_area, size),
        y: clamped.y,
        snapped_to_right: true,
    }
}

fn restore_ball_position(
    saved: Option<StoredWindowPosition>,
    work_area: WorkArea,
    size: WindowSize,
) -> StoredWindowPosition {
    match saved {
        Some(position) if position.snapped_to_right => {
            snap_ball_to_right(position, work_area, size)
        }
        Some(position) => {
            let mut clamped = clamp_window_position(position, work_area, size);
            clamped.snapped_to_right = false;
            clamped
        }
        None => snap_ball_to_right(
            StoredWindowPosition {
                x: right_edge_x(work_area, size),
                y: center_y(work_area, size),
                snapped_to_right: true,
            },
            work_area,
            size,
        ),
    }
}

fn settle_ball_position(
    position: StoredWindowPosition,
    work_area: WorkArea,
    size: WindowSize,
) -> StoredWindowPosition {
    let clamped = clamp_window_position(position, work_area, size);
    let distance_to_right = (right_edge_x(work_area, size) - clamped.x).abs();

    if distance_to_right <= RIGHT_SNAP_DISTANCE_PX {
        snap_ball_to_right(clamped, work_area, size)
    } else {
        StoredWindowPosition {
            snapped_to_right: false,
            ..clamped
        }
    }
}

fn clamp_panel_position(
    position: StoredWindowPosition,
    work_area: WorkArea,
    size: WindowSize,
) -> StoredWindowPosition {
    StoredWindowPosition {
        snapped_to_right: false,
        ..clamp_window_position(position, work_area, size)
    }
}

fn stored_position_from_physical(position: PhysicalPosition<i32>) -> StoredWindowPosition {
    StoredWindowPosition {
        x: position.x,
        y: position.y,
        snapped_to_right: false,
    }
}

fn window_size_from_physical(size: PhysicalSize<u32>) -> WindowSize {
    WindowSize {
        width: size.width.min(i32::MAX as u32) as i32,
        height: size.height.min(i32::MAX as u32) as i32,
    }
}

fn work_area_from_monitor(monitor: &tauri::Monitor) -> WorkArea {
    let work_area = monitor.work_area();
    WorkArea {
        x: work_area.position.x,
        y: work_area.position.y,
        width: work_area.size.width.min(i32::MAX as u32) as i32,
        height: work_area.size.height.min(i32::MAX as u32) as i32,
    }
}

fn window_state_path(app: &AppHandle) -> Result<PathBuf, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|err| format!("无法获取应用配置目录：{err}"))?;

    fs::create_dir_all(&config_dir).map_err(|err| format!("无法创建应用配置目录：{err}"))?;
    Ok(config_dir.join(WINDOW_STATE_FILE))
}

fn load_window_position_store(app: &AppHandle) -> WindowPositionStore {
    let Ok(path) = window_state_path(app) else {
        return WindowPositionStore::default();
    };

    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_window_position_store(app: &AppHandle, store: &WindowPositionStore) -> Result<(), String> {
    let path = window_state_path(app)?;
    let content =
        serde_json::to_string_pretty(store).map_err(|err| format!("窗口位置序列化失败：{err}"))?;
    fs::write(path, content).map_err(|err| format!("窗口位置保存失败：{err}"))
}

fn save_managed_window_positions(app: &AppHandle) {
    let Some(position_store) = app.try_state::<Mutex<WindowPositionStore>>() else {
        return;
    };
    let Some(store) = position_store.lock().ok().map(|store| store.clone()) else {
        return;
    };
    let _ = save_window_position_store(app, &store);
}

fn start_window_position_save_worker(app: AppHandle) -> WindowPositionSaveState {
    let (sender, receiver) = mpsc::channel::<()>();
    std::thread::spawn(move || loop {
        if receiver.recv().is_err() {
            break;
        }

        loop {
            match receiver.recv_timeout(WINDOW_POSITION_SAVE_DEBOUNCE) {
                Ok(()) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    save_managed_window_positions(&app);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    save_managed_window_positions(&app);
                    return;
                }
            }
        }
    });

    WindowPositionSaveState { sender }
}

fn monitor_for_window(window: &Window) -> Option<tauri::Monitor> {
    window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())
}

fn monitor_for_webview_window(window: &WebviewWindow) -> Option<tauri::Monitor> {
    window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())
}

fn restore_window_positions(app: &AppHandle) {
    let store = app
        .state::<Mutex<WindowPositionStore>>()
        .lock()
        .ok()
        .map(|store| store.clone());
    let Some(store) = store else {
        return;
    };

    if let Some(window) = app.get_webview_window(BALL_WINDOW_LABEL) {
        if let (Some(monitor), Ok(size)) = (
            window
                .current_monitor()
                .ok()
                .flatten()
                .or_else(|| window.primary_monitor().ok().flatten()),
            window.outer_size(),
        ) {
            let position = restore_ball_position(
                store.ball,
                work_area_from_monitor(&monitor),
                window_size_from_physical(size),
            );
            let _ = window.set_position(Position::Physical(PhysicalPosition::new(
                position.x, position.y,
            )));
        }
    }

    for (label, saved_position) in [
        (MAIN_WINDOW_LABEL, store.main),
        (SETTINGS_WINDOW_LABEL, store.settings),
    ] {
        let Some(position) = saved_position else {
            continue;
        };
        let Some(window) = app.get_webview_window(label) else {
            continue;
        };
        let Some(monitor) = window
            .current_monitor()
            .ok()
            .flatten()
            .or_else(|| window.primary_monitor().ok().flatten())
        else {
            continue;
        };
        let Ok(size) = window.outer_size() else {
            continue;
        };
        let position = clamp_panel_position(
            position,
            work_area_from_monitor(&monitor),
            window_size_from_physical(size),
        );
        let _ = window.set_position(Position::Physical(PhysicalPosition::new(
            position.x, position.y,
        )));
    }
}

fn handle_window_moved(window: &Window, position: PhysicalPosition<i32>) {
    let label = window.label();
    if ![BALL_WINDOW_LABEL, MAIN_WINDOW_LABEL, SETTINGS_WINDOW_LABEL].contains(&label) {
        return;
    }

    let Some(monitor) = monitor_for_window(window) else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };

    let work_area = work_area_from_monitor(&monitor);
    let size = window_size_from_physical(size);
    let stored = stored_position_from_physical(position);
    let settled = if label == BALL_WINDOW_LABEL {
        settle_ball_position(stored, work_area, size)
    } else {
        clamp_panel_position(stored, work_area, size)
    };

    if settled.x != position.x || settled.y != position.y {
        let _ = window.set_position(Position::Physical(PhysicalPosition::new(
            settled.x, settled.y,
        )));
    }

    let app = window.app_handle();
    // Release builds can receive a move event before setup has registered managed state.
    let Some(position_store) = app.try_state::<Mutex<WindowPositionStore>>() else {
        return;
    };
    let Ok(changed) = position_store
        .lock()
        .map(|mut store| store.set(label, settled))
    else {
        return;
    };

    if changed {
        if let Some(save_state) = app.try_state::<WindowPositionSaveState>() {
            let _ = save_state.sender.send(());
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreditsSnapshot {
    balance: Option<String>,
    has_credits: bool,
    unlimited: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindow {
    resets_at: Option<i64>,
    used_percent: i32,
    window_duration_mins: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitSnapshot {
    credits: Option<CreditsSnapshot>,
    limit_id: Option<String>,
    limit_name: Option<String>,
    plan_type: Option<String>,
    primary: Option<RateLimitWindow>,
    rate_limit_reached_type: Option<String>,
    secondary: Option<RateLimitWindow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GetAccountRateLimitsResponse {
    rate_limits: RateLimitSnapshot,
    rate_limits_by_limit_id: Option<HashMap<String, RateLimitSnapshot>>,
}

fn normalize_ball_size(size: u32) -> u32 {
    size.clamp(BALL_MIN_SIZE, BALL_MAX_SIZE)
}

#[derive(Default)]
struct RateLimitsCache {
    value: Option<CachedRateLimits>,
}

struct CachedRateLimits {
    fetched_at: Instant,
    proxy_url: Option<String>,
    value: GetAccountRateLimitsResponse,
}

#[derive(Debug)]
enum CodexLauncher {
    Direct(PathBuf),
    #[cfg(not(windows))]
    ShScript(PathBuf),
    #[cfg(windows)]
    CmdScript(PathBuf),
    #[cfg(windows)]
    PowerShellScript(PathBuf),
}

impl CodexLauncher {
    fn command(&self) -> Command {
        match self {
            CodexLauncher::Direct(path) => {
                let mut command = Command::new(path);
                command.args(["app-server", "--listen", "stdio://"]);
                command
            }
            #[cfg(not(windows))]
            CodexLauncher::ShScript(path) => {
                let mut command = Command::new("sh");
                command.arg(path);
                command.args(["app-server", "--listen", "stdio://"]);
                command
            }
            #[cfg(windows)]
            CodexLauncher::CmdScript(path) => {
                let mut command = Command::new("cmd.exe");
                command
                    .args(["/d", "/c"])
                    .arg(path)
                    .args(["app-server", "--listen", "stdio://"]);
                command
            }
            #[cfg(windows)]
            CodexLauncher::PowerShellScript(path) => {
                let mut command = Command::new("powershell.exe");
                command
                    .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                    .arg(path)
                    .args(["app-server", "--listen", "stdio://"]);
                command
            }
        }
    }
}

#[cfg(windows)]
fn hide_child_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_child_console(_: &mut Command) {}

fn jsonrpc_line(id: i32, method: &str, params: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

fn launcher_for_path(path: PathBuf) -> CodexLauncher {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match extension.as_str() {
        #[cfg(not(windows))]
        "sh" => CodexLauncher::ShScript(path),
        #[cfg(windows)]
        "cmd" | "bat" => CodexLauncher::CmdScript(path),
        #[cfg(windows)]
        "ps1" => CodexLauncher::PowerShellScript(path),
        _ => CodexLauncher::Direct(path),
    }
}

fn push_dir(candidates: &mut Vec<PathBuf>, value: Option<OsString>) {
    if let Some(value) = value {
        if !value.is_empty() {
            candidates.push(PathBuf::from(value));
        }
    }
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(path) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path));
    }

    #[cfg(not(windows))]
    {
        push_dir(
            &mut dirs,
            env::var_os("HOME").map(|value| {
                PathBuf::from(value)
                    .join(".local")
                    .join("bin")
                    .into_os_string()
            }),
        );
        push_dir(
            &mut dirs,
            env::var_os("HOME").map(|value| {
                PathBuf::from(value)
                    .join(".npm-global")
                    .join("bin")
                    .into_os_string()
            }),
        );
        push_dir(
            &mut dirs,
            env::var_os("HOME").map(|value| {
                PathBuf::from(value)
                    .join(".npm")
                    .join("bin")
                    .into_os_string()
            }),
        );
        push_dir(
            &mut dirs,
            env::var_os("HOME").map(|value| {
                PathBuf::from(value)
                    .join(".asdf")
                    .join("shims")
                    .into_os_string()
            }),
        );
        push_dir(
            &mut dirs,
            env::var_os("NPM_CONFIG_PREFIX")
                .map(|value| PathBuf::from(value).join("bin").into_os_string()),
        );
    }

    #[cfg(windows)]
    push_dir(
        &mut dirs,
        env::var_os("ProgramFiles")
            .map(|value| PathBuf::from(value).join("nodejs").into_os_string()),
    );
    push_dir(
        &mut dirs,
        env::var_os("ProgramFiles(x86)")
            .map(|value| PathBuf::from(value).join("nodejs").into_os_string()),
    );
    push_dir(
        &mut dirs,
        env::var_os("APPDATA").map(|value| PathBuf::from(value).join("npm").into_os_string()),
    );
    push_dir(
        &mut dirs,
        env::var_os("LOCALAPPDATA").map(|value| {
            PathBuf::from(value)
                .join("Programs")
                .join("nodejs")
                .into_os_string()
        }),
    );

    let mut unique = Vec::new();
    for dir in dirs {
        if !unique.iter().any(|existing: &PathBuf| existing == &dir) {
            unique.push(dir);
        }
    }
    unique
}

fn find_codex_launcher() -> Result<CodexLauncher, String> {
    if let Some(path) = env::var_os("CODEX_USAGE_BALL_CODEX_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(launcher_for_path(path));
        }
    }

    #[cfg(windows)]
    let names = ["codex.exe", "codex.cmd", "codex.bat", "codex.ps1"];
    #[cfg(not(windows))]
    let names = ["codex"];
    let mut tried = Vec::new();

    for dir in candidate_dirs() {
        for name in names {
            let candidate = dir.join(name);
            tried.push(candidate.display().to_string());
            if candidate.is_file() {
                return Ok(launcher_for_path(candidate));
            }
        }
    }

    Err(format!(
        "未找到 Codex CLI。请确认已安装 Codex，或设置 CODEX_USAGE_BALL_CODEX_PATH 指向 codex.cmd。已尝试 {} 个位置。",
        tried.len()
    ))
}

fn collect_stderr(rx: &mpsc::Receiver<String>) -> String {
    let lines: Vec<String> = rx.try_iter().collect();
    lines.join("\n")
}

fn normalize_proxy_url(proxy_url: Option<String>) -> Result<Option<String>, String> {
    let Some(proxy_url) = proxy_url else {
        return Ok(None);
    };
    let proxy_url = proxy_url.trim();
    if proxy_url.is_empty() {
        return Ok(None);
    }
    if proxy_url.len() > 2048 || proxy_url.chars().any(char::is_whitespace) {
        return Err("代理地址格式无效".to_string());
    }

    let scheme = proxy_url
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    if !matches!(
        scheme.as_deref(),
        Some("http" | "https" | "socks5" | "socks5h")
    ) {
        return Err("代理地址必须以 http://、https://、socks5:// 或 socks5h:// 开头".to_string());
    }

    Ok(Some(proxy_url.to_string()))
}

fn apply_proxy_environment(command: &mut Command, proxy_url: Option<&str>) {
    let Some(proxy_url) = proxy_url else {
        return;
    };

    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(name, proxy_url);
    }
}

fn spawn_codex_app_server(proxy_url: Option<&str>) -> Result<std::process::Child, String> {
    let launcher = find_codex_launcher()?;
    let mut command = launcher.command();
    apply_proxy_environment(&mut command, proxy_url);
    hide_child_console(&mut command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    command
        .spawn()
        .map_err(|err| format!("无法启动 codex app-server：{err}"))
}

fn show_window(app: &AppHandle, label: &str) -> Result<(), String> {
    let window = app
        .get_webview_window(label)
        .ok_or_else(|| format!("未找到窗口：{label}"))?;

    window.show().map_err(|err| err.to_string())?;
    let _ = window.unminimize();
    let _ = window.set_focus();
    if label == BALL_WINDOW_LABEL {
        set_ball_menu_checked(app, true);
    }
    Ok(())
}

fn hide_window(app: &AppHandle, label: &str) -> Result<(), String> {
    let window = app
        .get_webview_window(label)
        .ok_or_else(|| format!("未找到窗口：{label}"))?;

    window.hide().map_err(|err| err.to_string())?;
    if label == BALL_WINDOW_LABEL {
        set_ball_menu_checked(app, false);
    }
    Ok(())
}

fn set_ball_menu_checked(app: &AppHandle, checked: bool) {
    let app_state = app.state::<Mutex<TrayState>>();
    let Ok(state) = app_state.lock() else {
        return;
    };

    let Some(toggle_item) = state.ball_toggle_item.as_ref() else {
        return;
    };

    let _ = toggle_item.set_checked(checked);
}

fn toggle_ball_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(BALL_WINDOW_LABEL) else {
        set_ball_menu_checked(app, false);
        return;
    };

    if window.is_visible().unwrap_or(false) {
        let _ = hide_window(app, BALL_WINDOW_LABEL);
    } else {
        let _ = show_window(app, BALL_WINDOW_LABEL);
    }
}

fn install_tray(app: &AppHandle) -> tauri::Result<()> {
    let show_main = MenuItem::with_id(app, MENU_SHOW_MAIN, "显示主面板", true, None::<&str>)?;
    let toggle_ball = CheckMenuItem::with_id(
        app,
        MENU_TOGGLE_BALL,
        "显示悬浮球",
        true,
        true,
        None::<&str>,
    )?;
    let show_settings = MenuItem::with_id(app, MENU_SHOW_SETTINGS, "偏好设置", true, None::<&str>)?;
    if let Ok(mut tray_state) = app.state::<Mutex<TrayState>>().lock() {
        tray_state.ball_toggle_item = Some(toggle_ball.clone());
    }
    let separator = PredefinedMenuItem::separator(app)?;
    let exit = MenuItem::with_id(app, MENU_EXIT, "退出程序", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&show_main, &toggle_ball, &show_settings, &separator, &exit],
    )?;

    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("Codex 用量悬浮球")
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            MENU_SHOW_MAIN => {
                let _ = show_window(app, MAIN_WINDOW_LABEL);
            }
            MENU_TOGGLE_BALL => toggle_ball_window(app),
            MENU_SHOW_SETTINGS => {
                let _ = show_window(app, SETTINGS_WINDOW_LABEL);
            }
            MENU_EXIT => {
                save_managed_window_positions(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click { button, .. } if button == MouseButton::Left => {
                let _ = show_window(tray.app_handle(), MAIN_WINDOW_LABEL);
            }
            TrayIconEvent::DoubleClick { button, .. } if button == MouseButton::Left => {
                let _ = show_window(tray.app_handle(), MAIN_WINDOW_LABEL);
            }
            _ => {}
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}

#[tauri::command]
fn read_rate_limits(
    force: bool,
    proxy_url: Option<String>,
    cache: State<'_, Mutex<RateLimitsCache>>,
) -> Result<GetAccountRateLimitsResponse, String> {
    let proxy_url = normalize_proxy_url(proxy_url)?;
    // Both the floating ball and the hidden main window have timers. Keep the
    // cache lock during a fetch so simultaneous automatic reads share one query.
    let mut cache = cache
        .lock()
        .map_err(|_| "Codex 用量缓存暂时不可用".to_string())?;

    if !force {
        if let Some(cached) = cache.value.as_ref() {
            if cached.proxy_url == proxy_url && cached.fetched_at.elapsed() < RATE_LIMIT_CACHE_TTL {
                return Ok(cached.value.clone());
            }
        }
    }

    let value = fetch_rate_limits(proxy_url.as_deref())?;
    cache.value = Some(CachedRateLimits {
        fetched_at: Instant::now(),
        proxy_url,
        value: value.clone(),
    });
    Ok(value)
}

fn fetch_rate_limits(proxy_url: Option<&str>) -> Result<GetAccountRateLimitsResponse, String> {
    let mut child = spawn_codex_app_server(proxy_url)?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "无法写入 codex app-server 标准输入".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取 codex app-server 标准输出".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取 codex app-server 错误输出".to_string())?;

    let (stdout_tx, stdout_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if stdout_tx.send(line).is_err() {
                break;
            }
        }
    });

    let (stderr_tx, stderr_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if stderr_tx.send(line).is_err() {
                break;
            }
        }
    });

    let initialize = jsonrpc_line(
        1,
        "initialize",
        serde_json::json!({
            "clientInfo": {
                "name": "codex-usage-ball",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "experimentalApi": true
            }
        }),
    );

    writeln!(stdin, "{initialize}").map_err(|err| format!("初始化请求发送失败：{err}"))?;
    stdin
        .flush()
        .map_err(|err| format!("初始化请求刷新失败：{err}"))?;

    let mut initialized = false;

    loop {
        let line = match stdout_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => line,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = collect_stderr(&stderr_rx);
                if stderr.is_empty() {
                    return Err("读取 Codex 用量超时".to_string());
                }
                return Err(format!("读取 Codex 用量超时：{stderr}"));
            }
        };
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Codex 返回了无法解析的数据：{err}"));
            }
        };

        if message.get("id").and_then(Value::as_i64) == Some(1) && !initialized {
            initialized = true;
            let request = jsonrpc_line(2, "account/rateLimits/read", Value::Null);
            writeln!(stdin, "{request}").map_err(|err| format!("用量请求发送失败：{err}"))?;
            stdin
                .flush()
                .map_err(|err| format!("用量请求刷新失败：{err}"))?;
            continue;
        }

        if message.get("id").and_then(Value::as_i64) == Some(2) {
            let _ = child.kill();
            let _ = child.wait();

            if let Some(error) = message.get("error") {
                return Err(format!("Codex 用量接口返回错误：{error}"));
            }

            let result = message
                .get("result")
                .cloned()
                .ok_or_else(|| "Codex 用量接口缺少 result 字段".to_string())?;

            return serde_json::from_value(result)
                .map_err(|err| format!("Codex 用量结构解析失败：{err}"));
        }
    }
}

#[tauri::command]
fn show_main_panel(app: AppHandle) -> Result<(), String> {
    show_window(&app, MAIN_WINDOW_LABEL)
}

#[tauri::command]
fn hide_main_panel(app: AppHandle) -> Result<(), String> {
    hide_window(&app, MAIN_WINDOW_LABEL)
}

#[tauri::command]
fn hide_ball_window(app: AppHandle) -> Result<(), String> {
    hide_window(&app, BALL_WINDOW_LABEL)
}

#[tauri::command]
fn resize_ball_window(app: AppHandle, size: u32) -> Result<(), String> {
    let window = app
        .get_webview_window(BALL_WINDOW_LABEL)
        .ok_or_else(|| "找不到悬浮球窗口".to_string())?;
    let size = normalize_ball_size(size);

    let fallback_position = monitor_for_webview_window(&window).and_then(|monitor| {
        let current = window.outer_position().ok()?;
        let current_size = window.outer_size().ok()?;
        let work_area = work_area_from_monitor(&monitor);
        let current_size = window_size_from_physical(current_size);
        Some(StoredWindowPosition {
            x: current.x,
            y: current.y,
            snapped_to_right: (right_edge_x(work_area, current_size) - current.x).abs()
                <= RIGHT_SNAP_DISTANCE_PX,
        })
    });
    // The optimized frontend may invoke this command before setup finishes managing state.
    let saved_position = app
        .try_state::<Mutex<WindowPositionStore>>()
        .and_then(|position_store| position_store.lock().ok().and_then(|store| store.ball))
        .or(fallback_position);

    window
        .set_size(Size::Logical(LogicalSize::new(
            f64::from(size),
            f64::from(size + BALL_WINDOW_EXTRA_HEIGHT),
        )))
        .map_err(|err| format!("无法调整悬浮球窗口大小：{err}"))?;

    if let (Some(monitor), Ok(window_size)) =
        (monitor_for_webview_window(&window), window.outer_size())
    {
        let position = restore_ball_position(
            saved_position,
            work_area_from_monitor(&monitor),
            window_size_from_physical(window_size),
        );
        window
            .set_position(Position::Physical(PhysicalPosition::new(
                position.x, position.y,
            )))
            .map_err(|err| format!("无法恢复悬浮球位置：{err}"))?;
    }

    Ok(())
}

#[tauri::command]
fn show_settings_window(app: AppHandle) -> Result<(), String> {
    show_window(&app, SETTINGS_WINDOW_LABEL)
}

#[tauri::command]
fn hide_settings_window(app: AppHandle) -> Result<(), String> {
    hide_window(&app, SETTINGS_WINDOW_LABEL)
}

#[tauri::command]
fn exit_app(app: AppHandle) {
    save_managed_window_positions(&app);
    app.exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let position_store = load_window_position_store(app.handle());
            app.manage(Mutex::new(position_store));
            let position_save_state = start_window_position_save_worker(app.handle().clone());
            app.manage(position_save_state);
            app.manage(Mutex::new(TrayState::default()));
            app.manage(Mutex::new(RateLimitsCache::default()));
            restore_window_positions(app.handle());
            install_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::Moved(position) => handle_window_moved(window, *position),
            WindowEvent::CloseRequested { api, .. } => match window.label() {
                BALL_WINDOW_LABEL | MAIN_WINDOW_LABEL | SETTINGS_WINDOW_LABEL => {
                    api.prevent_close();
                    let _ = hide_window(&window.app_handle(), window.label());
                }
                _ => {}
            },
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            read_rate_limits,
            show_main_panel,
            hide_main_panel,
            hide_ball_window,
            resize_ball_window,
            show_settings_window,
            hide_settings_window,
            exit_app
        ])
        .run(tauri::generate_context!())
        .expect("Codex 用量悬浮球启动失败");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_area() -> WorkArea {
        WorkArea {
            x: 0,
            y: 24,
            width: 1920,
            height: 1040,
        }
    }

    fn ball_size() -> WindowSize {
        WindowSize {
            width: 112,
            height: 112,
        }
    }

    #[test]
    fn 小球没有保存位置时默认吸附在右侧边缘() {
        let position = restore_ball_position(None, work_area(), ball_size());

        assert_eq!(
            position,
            StoredWindowPosition {
                x: 1808,
                y: 488,
                snapped_to_right: true,
            }
        );
    }

    #[test]
    fn 小球拖离右侧边缘后保持自由位置() {
        let position = settle_ball_position(
            StoredWindowPosition {
                x: 640,
                y: 300,
                snapped_to_right: true,
            },
            work_area(),
            ball_size(),
        );

        assert_eq!(
            position,
            StoredWindowPosition {
                x: 640,
                y: 300,
                snapped_to_right: false,
            }
        );
    }

    #[test]
    fn 小球释放在右侧边缘附近时重新吸附() {
        let position = settle_ball_position(
            StoredWindowPosition {
                x: 1792,
                y: 300,
                snapped_to_right: false,
            },
            work_area(),
            ball_size(),
        );

        assert_eq!(
            position,
            StoredWindowPosition {
                x: 1808,
                y: 300,
                snapped_to_right: true,
            }
        );
    }

    #[test]
    fn 面板位置会被限制在显示器工作区内() {
        let position = clamp_panel_position(
            StoredWindowPosition {
                x: 1900,
                y: -80,
                snapped_to_right: false,
            },
            work_area(),
            WindowSize {
                width: 356,
                height: 580,
            },
        );

        assert_eq!(
            position,
            StoredWindowPosition {
                x: 1564,
                y: 24,
                snapped_to_right: false,
            }
        );
    }

    #[test]
    fn 悬浮球大小限制在支持范围内() {
        assert_eq!(normalize_ball_size(40), BALL_MIN_SIZE);
        assert_eq!(normalize_ball_size(112), 112);
        assert_eq!(normalize_ball_size(240), BALL_MAX_SIZE);
    }

    #[test]
    fn 支持常用代理协议并清理首尾空格() {
        assert_eq!(
            normalize_proxy_url(Some("  http://127.0.0.1:7890  ".to_string())).unwrap(),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(
            normalize_proxy_url(Some("socks5://127.0.0.1:1080".to_string())).unwrap(),
            Some("socks5://127.0.0.1:1080".to_string())
        );
    }

    #[test]
    fn 空代理等同于不启用代理() {
        assert_eq!(normalize_proxy_url(None).unwrap(), None);
        assert_eq!(normalize_proxy_url(Some("   ".to_string())).unwrap(), None);
    }

    #[test]
    fn 拒绝未知协议或包含空格的代理地址() {
        assert!(normalize_proxy_url(Some("ftp://127.0.0.1:21".to_string())).is_err());
        assert!(normalize_proxy_url(Some("http://127.0.0.1: 7890".to_string())).is_err());
    }
}
