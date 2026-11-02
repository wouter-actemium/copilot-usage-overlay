mod layout;
mod reminder;
mod tasks;
mod teams;

use chrono::{Datelike, Duration, Local, NaiveDate, Timelike};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::{env, fs, path::PathBuf, process::Command};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};
use tauri_plugin_opener::OpenerExt;

const API: &str = "https://api.github.com/copilot_internal/user";
const DAY_MS: i64 = 86_400_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Stats {
    total: i64,
    avg_all: f64,
    avg_week: f64,
    hours: Vec<i64>,
    daily: serde_json::Map<String, Value>,
    hit_at: Option<i64>,
    projected: Option<f64>,
    reset: i64,
    period_start: i64,
    ideal: f64,
    budget_per_day: f64,
    budget_per_day_total: f64,
    workdays_left: f64,
    days: f64,
}

#[derive(Serialize)]
struct State {
    used: i64,
    entitlement: i64,
    unlimited: bool,
    events: Vec<(i64, i64)>,
    stats: Stats,
    updated: i64,
}

fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

fn db_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map(|dir| dir.join("usage.db")).map_err(|e| e.to_string())
}

fn open_db(app: &AppHandle) -> Result<Connection, String> {
    let path = db_path(app)?;
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS events (ts INTEGER NOT NULL, delta INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
        CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER);
        CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);").map_err(|e| e.to_string())?;
    conn.execute_batch(tasks::SCHEMA).map_err(|e| e.to_string())?;
    Ok(conn)
}

fn copilot_editor_token() -> Option<String> {
    let base = env::var_os("LOCALAPPDATA").map(PathBuf::from)
        .or_else(|| env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
        .join("github-copilot");
    ["apps.json", "hosts.json"].iter().find_map(|name| {
        let json: Value = serde_json::from_str(&fs::read_to_string(base.join(name)).ok()?).ok()?;
        json.as_object()?.iter()
            .filter(|(key, _)| key.starts_with("github.com"))
            .find_map(|(_, v)| v.get("oauth_token").and_then(Value::as_str).map(str::to_owned))
    })
}

fn token() -> Result<String, String> {
    if let Ok(value) = env::var("GITHUB_TOKEN") {
        if !value.trim().is_empty() { return Ok(value.trim().to_owned()); }
    }
    if let Some(value) = copilot_editor_token() { return Ok(value); }
    let mut gh = Command::new("gh");
    gh.args(["auth", "token"]);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut gh, 0x0800_0000);
    let output = gh.output()
        .map_err(|e| format!("No token found: set GITHUB_TOKEN, sign in to Copilot in an editor, or install gh ({e})"))?;
    if !output.status.success() { return Err("gh auth token failed".into()); }
    String::from_utf8(output.stdout).map(|v| v.trim().to_owned()).map_err(|e| e.to_string())
}

fn is_workday(date: NaiveDate) -> bool {
    !matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun)
}

fn date_for(ts: i64) -> NaiveDate {
    chrono::DateTime::from_timestamp_millis(ts).unwrap_or_default().with_timezone(&Local).date_naive()
}

fn workdays_between(from: i64, to: i64) -> f64 {
    let mut t = from;
    let mut total = 0.0;
    while t < to {
        let date = date_for(t);
        let next = (date + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap().and_local_timezone(Local).single().map(|v| v.timestamp_millis()).unwrap_or(t + DAY_MS);
        let end = next.min(to);
        if is_workday(date) { total += (end - t) as f64 / DAY_MS as f64; }
        t = end;
    }
    total
}

fn add_workdays(from: i64, mut days: f64) -> Option<i64> {
    let mut t = from;
    for _ in 0..3650 {
        let date = date_for(t);
        let next = (date + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap().and_local_timezone(Local).single()?.timestamp_millis();
        if is_workday(date) {
            let available = (next - t) as f64 / DAY_MS as f64;
            if days <= available { return Some(t + (days * DAY_MS as f64) as i64); }
            days -= available;
        }
        t = next;
    }
    None
}

fn get_stats(conn: &Connection, now: i64, used: i64, entitlement: i64, unlimited: bool, reset: i64) -> Result<Stats, String> {
    let (total, first): (i64, Option<i64>) = conn.query_row("SELECT COALESCE(SUM(delta),0), MIN(ts) FROM events", [], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?;
    let mut daily_values = std::collections::HashMap::<NaiveDate, i64>::new();
    let mut hours = vec![0i64; 24];
    let mut stmt = conn.prepare("SELECT ts, delta FROM events").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).map_err(|e| e.to_string())?;
    for row in rows {
        let (ts, delta) = row.map_err(|e| e.to_string())?;
        *daily_values.entry(date_for(ts)).or_default() += delta;
        let hour = chrono::DateTime::from_timestamp_millis(ts).unwrap_or_default().with_timezone(&Local).hour() as usize;
        hours[hour] += delta;
    }
    let active_all: Vec<i64> = daily_values.iter().filter(|(d, v)| is_workday(**d) && **v > 0).map(|(_, v)| *v).collect();
    let week_start = Local::now().date_naive() - Duration::days(6);
    let active_week: Vec<i64> = daily_values.iter().filter(|(d, v)| **d >= week_start && is_workday(**d) && **v > 0).map(|(_, v)| *v).collect();
    let avg = |values: &[i64]| values.iter().sum::<i64>() as f64 / values.len().max(1) as f64;
    let mut daily = serde_json::Map::new();
    for (date, value) in daily_values { daily.insert(date.to_string(), Value::from(value)); }
    let period_start = reset - 31 * DAY_MS;
    let period_wd = workdays_between(period_start, reset);
    let ideal = if period_wd > 0.0 { entitlement as f64 * (workdays_between(period_start, now) / period_wd).min(1.0) } else { 0.0 };
    let workdays_left = workdays_between(now, reset);
    let budget = if workdays_left > 0.0 { ((entitlement - used).max(0) as f64) / workdays_left } else { 0.0 };
    let budget_total = if period_wd > 0.0 { entitlement as f64 / period_wd } else { 0.0 };
    let hit_at = if !unlimited && entitlement > 0 && avg(&active_week) > 0.0 { add_workdays(now, (entitlement - used) as f64 / avg(&active_week)) } else { None };
    let projected = if hit_at.is_some() { Some(used as f64 + avg(&active_week) * workdays_left) } else { None };
    Ok(Stats { total, avg_all: avg(&active_all), avg_week: avg(&active_week), hours, daily, hit_at, projected, reset, period_start, ideal, budget_per_day: budget, budget_per_day_total: budget_total, workdays_left, days: first.map_or(0.0, |v| (now - v) as f64 / DAY_MS as f64) })
}

#[tauri::command]
fn refresh(app: AppHandle) -> Result<State, String> {
    let client = reqwest::blocking::Client::new();
    let response: Value = client.get(API).bearer_auth(token()?).header("Accept", "application/json").header("User-Agent", "copilot-overlay").send().map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?.json().map_err(|e| e.to_string())?;
    let quota = response.get("quota_snapshots").and_then(|v| v.get("premium_interactions")).ok_or("premium_interactions ontbreekt")?;
    let unlimited = quota.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
    let entitlement = quota.get("entitlement").and_then(Value::as_i64).unwrap_or(0);
    let remaining = quota.get("remaining").and_then(Value::as_i64).unwrap_or(0);
    let used = entitlement - remaining;
    let now = now_ms();
    let conn = open_db(&app)?;
    let last: Option<i64> = conn.query_row("SELECT value FROM meta WHERE key='last'", [], |r| r.get(0)).ok();
    if !unlimited {
        if let Some(previous) = last { if used > previous { conn.execute("INSERT INTO events (ts,delta) VALUES (?1,?2)", params![now, used - previous]).map_err(|e| e.to_string())?; } }
        conn.execute("INSERT OR REPLACE INTO meta (key,value) VALUES ('last',?1)", params![used]).map_err(|e| e.to_string())?;
    }
    let mut event_stmt = conn.prepare("SELECT ts,delta FROM events WHERE ts > ?1 ORDER BY ts").map_err(|e| e.to_string())?;
    let events = event_stmt.query_map(params![now - 7 * DAY_MS], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    let reset = response.get("quota_reset_date").and_then(Value::as_str).and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok()).map(|v| v.timestamp_millis()).unwrap_or_else(|| {
        let date = Local::now().date_naive();
        (date.with_day(1).unwrap() + Duration::days(32)).with_day(1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_local_timezone(Local).single().unwrap().timestamp_millis()
    });
    Ok(State { used, entitlement, unlimited, events, stats: get_stats(&conn, now, used, entitlement, unlimited, reset)?, updated: now })
}

#[tauri::command]
fn close_window(app: AppHandle) -> Result<(), String> {
    app.get_webview_window("main").ok_or("main window not found")?.hide().map_err(|e| e.to_string())
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        // Showing a window on Windows brings its taskbar button back.
        let _ = window.set_skip_taskbar(true);
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[tauri::command]
fn start_dragging(app: AppHandle) -> Result<(), String> {
    app.get_webview_window("main").ok_or("main window not found")?.start_dragging().map_err(|e| e.to_string())
}

#[tauri::command]
fn load_settings(app: AppHandle) -> Result<Option<String>, String> {
    open_db(&app)?.query_row("SELECT value FROM settings WHERE key='ui'", [], |r| r.get(0)).optional().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_settings(app: AppHandle, json: String) -> Result<(), String> {
    open_db(&app)?.execute("INSERT OR REPLACE INTO settings (key,value) VALUES ('ui',?1)", params![json]).map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_tasks(app: AppHandle) -> Result<Vec<tasks::Task>, String> {
    tasks::list(&open_db(&app)?)
}

#[tauri::command]
async fn sync_tasks(app: AppHandle, options: tasks::SyncOptions) -> Result<tasks::SyncResult, String> {
    tauri::async_runtime::spawn_blocking(move || Ok(tasks::sync(&mut open_db(&app)?, now_ms(), &options)))
        .await.map_err(|e| e.to_string())?
}

#[tauri::command]
fn dismiss_task(app: AppHandle, id: String) -> Result<(), String> {
    tasks::dismiss(&open_db(&app)?, &id)
}

// Opens the stored URL by task id so the webview can never request an arbitrary link.
#[tauri::command]
async fn open_task(app: AppHandle, id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let url = tasks::url(&open_db(&app)?, &id)?;
        // `app:<path>` is only ever written by a task source on the backend, never by the webview.
        if let Some(path) = url.strip_prefix("app:") { return reminder::open_app(path); }
        if !(url.starts_with("https://") || url == "msteams:") { return Err("only https, msteams or app links can be opened".into()); }
        app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
    }).await.map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = dotenvy::dotenv();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_) => layout::schedule_save(window.app_handle()),
            _ => {}
        })
        .setup(|app| {
            let _ = layout::restore(app.handle());
            if let Some(window) = app.get_webview_window("main") { let _ = window.set_skip_taskbar(true); }
            let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let reset = MenuItem::with_id(app, "reset", "Reset position and size", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &reset, &quit])?;
            let mut tray = TrayIconBuilder::new()
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "reset" => {
                        show_main(app);
                        let _ = layout::reset(app);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() { tray = tray.icon(icon.clone()); }
            tray.build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![refresh, close_window, start_dragging, layout::resize_window, load_settings, save_settings, list_tasks, sync_tasks, dismiss_task, open_task])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
