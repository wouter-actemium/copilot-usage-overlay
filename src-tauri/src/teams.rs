//! Teams source: the unread count comes from Teams' own taskbar badge, details from the Windows toasts Teams raised.
//! No Teams sign-in is needed, but only unread items Teams reports to the taskbar are seen.

use crate::tasks::{Task, TaskSource};
use reqwest::blocking::Client;
use rusqlite::{Connection, OpenFlags};
use std::{env, fs, path::PathBuf, process::Command, time::{SystemTime, UNIX_EPOCH}};

/// 100 ns ticks between 1601-01-01 (the Windows FILETIME epoch) and the Unix epoch.
const FILETIME_UNIX_OFFSET: i64 = 116_444_736_000_000_000;
const DB_FILES: [&str; 3] = ["wpndatabase.db", "wpndatabase.db-wal", "wpndatabase.db-shm"];

/// Prints `TEAMS|<help text>` for the Teams taskbar button, which reads like "Attention requested, 2 items, ...".
const TASKBAR_SCRIPT: &str = "Add-Type -AssemblyName UIAutomationClient; Add-Type -AssemblyName UIAutomationTypes; \
    $root = [System.Windows.Automation.AutomationElement]::RootElement; \
    $class = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty, 'Shell_TrayWnd'); \
    $tray = $root.FindFirst('Children', $class); if (-not $tray) { exit 2 }; \
    foreach ($e in $tray.FindAll('Descendants', [System.Windows.Automation.Condition]::TrueCondition)) { \
        if ($e.Current.AutomationId -like 'Appid: MSTeams*') { Write-Output ('TEAMS|' + $e.Current.HelpText); exit 0 } }; \
    exit 2";

pub struct TeamsNotifications;

/// `None` when Teams has no taskbar button (not running), so its state is unknown.
#[cfg(windows)]
fn taskbar_unread() -> Result<Option<u32>, String> {
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", TASKBAR_SCRIPT]);
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
    let output = command.output().map_err(|e| format!("cannot query the taskbar ({e})"))?;
    if output.status.code() == Some(2) { return Ok(None); }
    if !output.status.success() { return Err("taskbar query failed".into()); }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let help = stdout.trim().strip_prefix("TEAMS|").unwrap_or("");
    Ok(Some(help.split(|c: char| !c.is_ascii_digit()).find(|n| !n.is_empty()).and_then(|n| n.parse().ok()).unwrap_or(0)))
}

#[cfg(not(windows))]
fn taskbar_unread() -> Result<Option<u32>, String> { Ok(None) }

fn decode(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The visible lines of a toast, without the app attribution line.
fn toast_lines(xml: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<text") {
        rest = &rest[start + "<text".len()..];
        let Some(open_end) = rest.find('>') else { break };
        let attributes = &rest[..open_end];
        rest = &rest[open_end + 1..];
        if attributes.ends_with('/') { continue; }
        let Some(close) = rest.find("</text>") else { break };
        let line = unescape(rest[..close].trim());
        if !attributes.contains("attribution") && !line.is_empty() { lines.push(line); }
        rest = &rest[close + "</text>".len()..];
    }
    lines
}

fn task(id: String, title: String, subtitle: String) -> Task {
    Task { id, source: "teams".into(), badge: "TM".into(), title, subtitle, url: "msteams:".into() }
}

/// Newest unexpired Teams toasts, up to `limit`.
fn toasts(conn: &Connection, limit: usize) -> Result<Vec<Task>, String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis() as i64 * 10_000 + FILETIME_UNIX_OFFSET;
    let mut stmt = conn.prepare(
        "SELECT n.Id, n.ExpiryTime, n.Payload FROM Notification n JOIN NotificationHandler h ON h.RecordId = n.HandlerId
         WHERE h.PrimaryId LIKE '%Teams%' AND n.Type = 'toast' ORDER BY n.ArrivalTime DESC",
    ).map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<Vec<u8>>>(2)?)))
        .map_err(|e| e.to_string())?;

    let mut tasks = Vec::new();
    for row in rows {
        let (id, expiry, payload) = row.map_err(|e| e.to_string())?;
        if expiry.unwrap_or(0) > 0 && expiry.unwrap_or(0) <= now { continue; }
        let mut lines = toast_lines(&decode(&payload.unwrap_or_default()));
        if lines.is_empty() { continue; }
        let sender = lines.remove(0);
        let (title, subtitle) = if lines.is_empty() { (sender, "Teams".to_owned()) } else { (lines.join(" "), sender) };
        tasks.push(task(format!("teams:{id}"), title, subtitle));
        if tasks.len() == limit { break; }
    }
    Ok(tasks)
}

fn recent_toasts(limit: usize) -> Result<Vec<Task>, String> {
    let dir = env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Microsoft").join("Windows").join("Notifications"))
        .filter(|d| d.join(DB_FILES[0]).exists()).ok_or("no notification database")?;
    // Read a private copy so the live system database is never locked or modified.
    let scratch = env::temp_dir().join(format!("copilot-overlay-wpn-{}", std::process::id()));
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let result = (|| {
        for name in DB_FILES {
            if let Err(e) = fs::copy(dir.join(name), scratch.join(name)) {
                if name == DB_FILES[0] { return Err(e.to_string()); }
            }
        }
        let conn = Connection::open_with_flags(scratch.join(DB_FILES[0]), OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
        toasts(&conn, limit)
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

impl TaskSource for TeamsNotifications {
    fn id(&self) -> &'static str { "teams" }

    fn fetch(&self, _client: &Client) -> Result<Option<Vec<Task>>, String> {
        let Some(count) = taskbar_unread()? else { return Ok(None) };
        if count == 0 { return Ok(Some(Vec::new())); }
        // Toasts add sender and text when available; the badge count alone still yields a task.
        let mut tasks = recent_toasts(count as usize).unwrap_or_default();
        if tasks.is_empty() {
            tasks.push(task(format!("teams:badge:{count}"), format!("{count} unread in Teams"), "Teams".to_owned()));
        }
        Ok(Some(tasks))
    }
}
