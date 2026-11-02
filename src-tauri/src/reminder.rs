//! Time-based task source: reminds to book hours in TRM on a configurable schedule, within working hours.

use crate::tasks::{Task, TaskSource};
use chrono::{Datelike, Local, NaiveDateTime, NaiveTime, Timelike, Weekday};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::{env, io::Write, path::Path, process::{Command, Stdio}};

const DEFAULT_APP: &str = r"C:\Planning\Planning.WinUI.exe";

/// Restores and focuses the main window of the process named in `FOCUS_PROCESS`; exits 2 when it has no window.
const FOCUS_SCRIPT: &str = r#"
$p = Get-Process -Name $env:FOCUS_PROCESS -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { exit 2 }
Add-Type -Namespace Win -Name Native -MemberDefinition '[DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr h, int c); [DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr h); [DllImport("user32.dll")] public static extern bool IsIconic(System.IntPtr h); [DllImport("user32.dll")] public static extern void keybd_event(byte k, byte s, int f, System.UIntPtr e);'
if ([Win.Native]::IsIconic($p.MainWindowHandle)) { [Win.Native]::ShowWindow($p.MainWindowHandle, 9) | Out-Null }
[Win.Native]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero); [Win.Native]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
[Win.Native]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
"#;

/// Brings an already running instance of the app at `path` to the front; `false` when none has a window.
#[cfg(windows)]
pub fn focus_running(path: &str) -> bool {
    let Some(name) = Path::new(path).file_stem().and_then(|s| s.to_str()) else { return false };
    let mut command = Command::new("powershell.exe");
    // The script goes through stdin because its double quotes do not survive being passed as an argument.
    command.args(["-NoProfile", "-NonInteractive", "-Command", "-"]).env("FOCUS_PROCESS", name).stdin(Stdio::piped());
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
    let Ok(mut child) = command.spawn() else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(FOCUS_SCRIPT.as_bytes()).is_err() { return false; }
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(not(windows))]
pub fn focus_running(_path: &str) -> bool { false }

/// Focuses a running instance, otherwise starts the app straight on the hours screen (`/trm`).
pub fn open_app(path: &str) -> Result<(), String> {
    if focus_running(path) { return Ok(()); }
    let mut command = Command::new(path);
    command.arg("/trm");
    if let Some(dir) = Path::new(path).parent() { command.current_dir(dir); }
    command.spawn().map(|_| ()).map_err(|e| format!("cannot start {path} ({e})"))
}

#[derive(Deserialize, Clone)]
pub struct Reminder {
    /// Comma separated: fixed times (`11:30`), `hourly`, or `every 90 min` / `every 2 h`.
    pub schedule: String,
    /// `HH:MM-HH:MM`, weekdays only.
    pub hours: String,
}

pub struct TrmReminder(pub Reminder);

fn parse_time(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text.trim(), "%H:%M").ok()
}

fn parse_hours(text: &str) -> Result<(NaiveTime, NaiveTime), String> {
    let range = text.split_once('-').and_then(|(from, to)| Some((parse_time(from)?, parse_time(to)?)));
    match range {
        Some((start, end)) if start < end => Ok((start, end)),
        _ => Err(format!("invalid working hours '{text}', expected like 08:00-17:00")),
    }
}

fn interval_minutes(token: &str) -> Option<u32> {
    if token == "hourly" { return Some(60); }
    let rest = token.strip_prefix("every")?.trim();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let amount: u32 = digits.parse().ok().filter(|n| *n > 0)?;
    match rest[digits.len()..].trim().chars().next() {
        Some('h') => Some(amount * 60),
        Some('m') | None => Some(amount),
        _ => None,
    }
}

/// Sorted reminder times inside the working hours; intervals count from the start of the working hours.
fn slots(schedule: &str, start: NaiveTime, end: NaiveTime) -> Result<Vec<NaiveTime>, String> {
    let mut slots = Vec::new();
    for token in schedule.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
        if let Some(time) = parse_time(&token) {
            slots.push(time);
        } else if let Some(step) = interval_minutes(&token) {
            let mut minute = start.hour() * 60 + start.minute();
            while minute <= end.hour() * 60 + end.minute() {
                slots.push(NaiveTime::from_hms_opt(minute / 60, minute % 60, 0).ok_or("invalid time")?);
                minute += step;
            }
        } else {
            return Err(format!("invalid schedule entry '{token}', expected like 11:30, hourly or every 90 min"));
        }
    }
    slots.retain(|s| (start..=end).contains(s));
    slots.sort();
    slots.dedup();
    if slots.is_empty() { return Err("schedule has no time inside the working hours".into()); }
    Ok(slots)
}

/// The most recent reminder time that has passed today, or `None` outside working hours.
fn current_slot(slots: &[NaiveTime], start: NaiveTime, end: NaiveTime, now: NaiveDateTime) -> Option<NaiveTime> {
    if matches!(now.weekday(), Weekday::Sat | Weekday::Sun) || !(start..=end).contains(&now.time()) { return None; }
    slots.iter().rev().find(|slot| **slot <= now.time()).copied()
}

impl TaskSource for TrmReminder {
    fn id(&self) -> &'static str { "trm" }

    /// At most one reminder exists at a time: a newer slot replaces an older one that was not dismissed.
    fn fetch(&self, _client: &Client) -> Result<Option<Vec<Task>>, String> {
        let (start, end) = parse_hours(&self.0.hours)?;
        let slots = slots(&self.0.schedule, start, end)?;
        let now = Local::now().naive_local();
        let Some(slot) = current_slot(&slots, start, end, now) else { return Ok(Some(Vec::new())) };
        let app = env::var("TRM_PATH").ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()).unwrap_or_else(|| DEFAULT_APP.to_owned());
        Ok(Some(vec![Task {
            id: format!("trm:{}:{}", now.date(), slot.format("%H%M")),
            source: "trm".into(),
            badge: "TRM".into(),
            title: "Update your worked hours".into(),
            subtitle: format!("TRM · {}", slot.format("%H:%M")),
            url: format!("app:{app}"),
        }]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn t(h: u32, m: u32) -> NaiveTime { NaiveTime::from_hms_opt(h, m, 0).unwrap() }
    fn at(day: u32, h: u32, m: u32) -> NaiveDateTime { NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_time(t(h, m)) }

    #[test]
    fn fixed_times_and_intervals() {
        let (start, end) = (t(8, 0), t(17, 0));
        assert_eq!(slots("15:30, 11:30, 11:30", start, end).unwrap(), vec![t(11, 30), t(15, 30)]);
        assert_eq!(slots("hourly", start, end).unwrap().len(), 10);
        assert_eq!(slots("every 90 min", start, end).unwrap(), vec![t(8, 0), t(9, 30), t(11, 0), t(12, 30), t(14, 0), t(15, 30), t(17, 0)]);
        assert_eq!(slots("every 4 h", start, end).unwrap(), vec![t(8, 0), t(12, 0), t(16, 0)]);
        assert_eq!(slots("20:00, 11:30", start, end).unwrap(), vec![t(11, 30)]);
        assert!(slots("sometimes", start, end).is_err());
        assert!(slots("20:00", start, end).is_err());
    }

    #[test]
    fn working_hours() {
        assert!(parse_hours("08:00-17:00").is_ok());
        assert!(parse_hours("17:00-08:00").is_err());
        assert!(parse_hours("morning").is_err());
    }

    #[test]
    fn current_slot_is_latest_passed_within_working_hours() {
        let (start, end) = (t(8, 0), t(17, 0));
        let slots = [t(11, 30), t(15, 30)];
        // 2026-10-27 is a Tuesday, 2026-10-31 a Saturday.
        assert_eq!(current_slot(&slots, start, end, at(27, 10, 0)), None);
        assert_eq!(current_slot(&slots, start, end, at(27, 11, 30)), Some(t(11, 30)));
        assert_eq!(current_slot(&slots, start, end, at(27, 14, 59)), Some(t(11, 30)));
        assert_eq!(current_slot(&slots, start, end, at(27, 16, 0)), Some(t(15, 30)));
        assert_eq!(current_slot(&slots, start, end, at(27, 17, 30)), None);
        assert_eq!(current_slot(&slots, start, end, at(31, 12, 0)), None);
    }
}
