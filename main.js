const { app, BrowserWindow, ipcMain } = require('electron');
const { execFileSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { DatabaseSync } = require('node:sqlite');

// Minimal .env loader (KEY=VALUE lines); existing env vars win.
try {
  for (const line of fs.readFileSync(path.join(__dirname, '.env'), 'utf8').split(/\r?\n/)) {
    const m = line.match(/^\s*([\w.]+)\s*=\s*["']?(.*?)["']?\s*$/);
    if (m && !(m[1] in process.env)) process.env[m[1]] = m[2];
  }
} catch {}

const API = 'https://api.github.com/copilot_internal/user';
const DAY = 24 * 3600 * 1000;
let win;
let db;

// Full history lives in usage.db (events: ts ms, delta); meta holds the last seen 'used'.
function openDb() {
  fs.mkdirSync(app.getPath('userData'), { recursive: true });
  db = new DatabaseSync(path.join(app.getPath('userData'), 'usage.db'));
  db.exec(`CREATE TABLE IF NOT EXISTS events (ts INTEGER NOT NULL, delta INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER);`);

  // One-time import of the old JSON store.
  const old = path.join(app.getPath('userData'), 'usage.json');
  if (fs.existsSync(old)) {
    try {
      const j = JSON.parse(fs.readFileSync(old, 'utf8'));
      const ins = db.prepare('INSERT INTO events (ts, delta) VALUES (?, ?)');
      for (const [t, d] of j.events || []) ins.run(t, d);
      if (j.last != null) setLast(j.last);
      fs.renameSync(old, old + '.imported');
    } catch {}
  }
}

const getLast = () => db.prepare("SELECT value FROM meta WHERE key = 'last'").get()?.value ?? null;
const setLast = (v) => db.prepare("INSERT OR REPLACE INTO meta (key, value) VALUES ('last', ?)").run(v);

function getToken() {
  if (process.env.GITHUB_TOKEN) return process.env.GITHUB_TOKEN.trim();
  return execFileSync('gh', ['auth', 'token'], { encoding: 'utf8', shell: process.platform === 'win32' }).trim();
}

function send(state) {
  if (win && !win.isDestroyed()) win.webContents.send('state', state);
}

// Fractional count of Mon-Fri days between two timestamps (local time).
function workdaysBetween(a, b) {
  let sum = 0;
  for (let t = a; t < b;) {
    const next = new Date(t);
    next.setHours(24, 0, 0, 0);
    const end = Math.min(next.getTime(), b);
    if (new Date(t).getDay() % 6) sum += (end - t) / DAY;
    t = end;
  }
  return sum;
}

// Timestamp reached after spending n workdays from `from`, skipping weekends.
function addWorkdays(from, n) {
  let t = from;
  for (let i = 0; i < 3650; i++) {
    const next = new Date(t);
    next.setHours(24, 0, 0, 0);
    const nt = next.getTime();
    if (new Date(t).getDay() % 6) {
      const avail = (nt - t) / DAY;
      if (n <= avail) return t + n * DAY;
      n -= avail;
    }
    t = nt;
  }
  return null;
}

function getStats(now, used, entitlement, unlimited, resetDate) {
  const WD = "strftime('%w', ts / 1000, 'unixepoch', 'localtime') NOT IN ('0', '6')";
  const { total, first } = db.prepare(`SELECT COALESCE(SUM(delta), 0) AS total, MIN(ts) AS first FROM events`).get();
  const spanDays = first ? (now - first) / DAY : 0;
  const isWorkday = (date) => { const day = new Date(`${date}T12:00:00`).getDay(); return day > 0 && day < 6; };
  const allDays = db.prepare("SELECT strftime('%Y-%m-%d', ts / 1000, 'unixepoch', 'localtime') AS d, SUM(delta) AS s FROM events GROUP BY d").all();
  const activeAllDays = allDays.filter((r) => r.s > 0 && isWorkday(r.d));
  const weekDate = new Date(now - 7 * DAY);
  const weekKey = `${weekDate.getFullYear()}-${String(weekDate.getMonth() + 1).padStart(2, '0')}-${String(weekDate.getDate()).padStart(2, '0')}`;
  const weekDays = allDays.filter((r) => r.d >= weekKey);
  const activeWeekDays = weekDays.filter((r) => r.s > 0 && isWorkday(r.d));
  const avgAll = activeAllDays.length ? activeAllDays.reduce((sum, r) => sum + r.s, 0) / activeAllDays.length : 0;
  const avgWeek = activeWeekDays.length ? activeWeekDays.reduce((sum, r) => sum + r.s, 0) / activeWeekDays.length : 0;

  const hours = Array(24).fill(0);
  const rows = db.prepare("SELECT CAST(strftime('%H', ts / 1000, 'unixepoch', 'localtime') AS INTEGER) AS h, SUM(delta) AS s FROM events GROUP BY h").all();
  for (const r of rows) hours[r.h] = r.s;

  const d = new Date(now);
  const reset = Date.parse(resetDate) || Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 1);
  let hitAt = null;
  let projected = null;
  const daily = {};
  for (const r of db.prepare("SELECT strftime('%Y-%m-%d', ts / 1000, 'unixepoch', 'localtime') AS d, SUM(delta) AS s FROM events WHERE ts > ? GROUP BY d").all(now - 19 * 7 * DAY)) daily[r.d] = r.s;
  const rd = new Date(reset);
  const periodStart = Date.UTC(rd.getUTCFullYear(), rd.getUTCMonth() - 1, rd.getUTCDate());
  const periodWd = workdaysBetween(periodStart, reset);
  const ideal = periodWd > 0 ? entitlement * Math.min(1, workdaysBetween(periodStart, now) / periodWd) : 0;
  const workdaysLeft = workdaysBetween(now, reset);
  const budgetPerDay = workdaysLeft > 0 ? Math.max(0, entitlement - used) / workdaysLeft : 0;
  if (!unlimited && entitlement && avgWeek > 0) {
    hitAt = addWorkdays(now, (entitlement - used) / avgWeek);
    projected = used + avgWeek * workdaysLeft;
  }
  return { total, avgAll, avgWeek, hours, daily, hitAt, projected, reset, periodStart, ideal, budgetPerDay, workdaysLeft, days: spanDays };
}

async function poll() {
  try {
    const res = await fetch(API, {
      headers: { Authorization: `token ${getToken()}`, Accept: 'application/json', 'User-Agent': 'copilot-overlay' },
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const body = await res.json();
    const q = body.quota_snapshots?.premium_interactions;
    if (!q) throw new Error('premium_interactions ontbreekt');

    const unlimited = !!q.unlimited;
    const entitlement = q.entitlement || 0;
    const used = entitlement - (q.remaining || 0);
    const now = Date.now();

    if (!unlimited) {
      const last = getLast();
      if (last !== null && used > last) db.prepare('INSERT INTO events (ts, delta) VALUES (?, ?)').run(now, used - last);
      setLast(used);
    }
    const events = db.prepare('SELECT ts, delta FROM events WHERE ts > ? ORDER BY ts').all(now - 7 * DAY).map((r) => [r.ts, r.delta]);
    const stats = getStats(now, used, entitlement, unlimited, body.quota_reset_date);
    send({ used, entitlement, unlimited, events, stats, updated: now });
  } catch (e) {
    send({ error: e.message });
  }
}

app.whenReady().then(() => {
  openDb();
  win = new BrowserWindow({
    width: 300,
    height: 600,
    frame: false,
    backgroundColor: '#14161a',
    alwaysOnTop: true,
    webPreferences: { nodeIntegration: true, contextIsolation: false },
  });
  win.setAlwaysOnTop(true, 'screen-saver');
  win.loadFile('index.html');
  win.webContents.on('did-finish-load', poll);
  ipcMain.on('refresh', poll);
  setInterval(poll, 60000);
});

app.on('window-all-closed', () => app.quit());
