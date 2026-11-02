//! Remembers the window position and size per monitor setup.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::open_db;

const DEFAULT_SIZE: (f64, f64) = (300.0, 600.0);
const SAVE_DELAY: Duration = Duration::from_millis(500);

static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
static EXPANDED: AtomicBool = AtomicBool::new(true);

#[derive(Serialize, Deserialize)]
struct Layout {
    x: i32,
    y: i32,
    width: u32,
    /// Only the expanded height is remembered; compact height follows the content.
    expanded_height: Option<u32>,
}

fn main_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window("main").ok_or_else(|| "main window not found".to_string())
}

/// Settings key that identifies the connected displays (position, size and scale of each).
fn layout_key(window: &WebviewWindow) -> Result<String, String> {
    let mut parts: Vec<String> = window.available_monitors().map_err(|e| e.to_string())?.iter().map(|m| {
        let (p, s) = (m.position(), m.size());
        format!("{},{} {}x{} @{}", p.x, p.y, s.width, s.height, m.scale_factor())
    }).collect();
    parts.sort();
    Ok(format!("window:{}", parts.join(";")))
}

fn load(app: &AppHandle, key: &str) -> Result<Option<Layout>, String> {
    let json: Option<String> = open_db(app)?
        .query_row("SELECT value FROM settings WHERE key=?1", params![key], |r| r.get(0))
        .optional().map_err(|e| e.to_string())?;
    Ok(json.and_then(|v| serde_json::from_str(&v).ok()))
}

fn save(app: &AppHandle) -> Result<(), String> {
    let window = main_window(app)?;
    // A minimized window reports a bogus off-screen position.
    if window.is_minimized().map_err(|e| e.to_string())? || !window.is_visible().map_err(|e| e.to_string())? { return Ok(()); }
    let key = layout_key(&window)?;
    let (pos, size) = (window.outer_position().map_err(|e| e.to_string())?, window.inner_size().map_err(|e| e.to_string())?);
    let previous = load(app, &key)?.and_then(|l| l.expanded_height);
    let expanded_height = if EXPANDED.load(Ordering::SeqCst) { Some(size.height) } else { previous };
    let json = serde_json::to_string(&Layout { x: pos.x, y: pos.y, width: size.width, expanded_height }).map_err(|e| e.to_string())?;
    open_db(app)?.execute("INSERT OR REPLACE INTO settings (key,value) VALUES (?1,?2)", params![key, json]).map(|_| ()).map_err(|e| e.to_string())
}

/// Called for every move/resize; writes once the window has been still for a moment.
pub fn schedule_save(app: &AppHandle) {
    let seq = SAVE_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(SAVE_DELAY);
        if SAVE_SEQ.load(Ordering::SeqCst) == seq { let _ = save(&app); }
    });
}

pub fn restore(app: &AppHandle) -> Result<(), String> {
    let window = main_window(app)?;
    let Some(layout) = load(app, &layout_key(&window)?)? else { return Ok(()) };
    let height = layout.expanded_height.unwrap_or(window.inner_size().map_err(|e| e.to_string())?.height);
    window.set_size(PhysicalSize::new(layout.width, height)).map_err(|e| e.to_string())?;
    window.set_position(PhysicalPosition::new(layout.x, layout.y)).map_err(|e| e.to_string())
}

pub fn reset(app: &AppHandle) -> Result<(), String> {
    let window = main_window(app)?;
    open_db(app)?.execute("DELETE FROM settings WHERE key=?1", params![layout_key(&window)?]).map_err(|e| e.to_string())?;
    window.set_size(LogicalSize::new(DEFAULT_SIZE.0, DEFAULT_SIZE.1)).map_err(|e| e.to_string())?;
    window.center().map_err(|e| e.to_string())?;
    // The frontend re-applies compact/expanded sizing.
    app.emit("layout-reset", ()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn resize_window(app: AppHandle, height: f64, expanded: bool) -> Result<(), String> {
    EXPANDED.store(expanded, Ordering::SeqCst);
    let window = main_window(&app)?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let width = window.inner_size().map_err(|e| e.to_string())?.to_logical::<f64>(scale).width;
    let remembered = if expanded { load(&app, &layout_key(&window)?)?.and_then(|l| l.expanded_height) } else { None };
    let height = remembered.map_or(height, |h| h as f64 / scale);
    window.set_size(LogicalSize::new(width, height)).map_err(|e| e.to_string())
}
