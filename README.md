# Copilot Overlay

Always-on-top Tauri overlay that shows your GitHub Copilot premium request usage, plus a small task list (pull requests, Teams messages, hour-logging reminders).

## Features

- Live usage with a daily budget, ideal-pace marker, hourly/weekly charts and a quota forecast
- Statistics tab with averages, busiest hours and expected usage at reset
- Task list from GitHub and Azure DevOps pull requests, unread Teams messages and TRM reminders; new tasks can flash until hovered
- Collapsible, optionally transparent until hovered, with a configurable border color to make the window easy to find
- Tray icon (show, reset position and size, exit); window position and size are remembered
- Dutch and English UI

All options are in the settings (gear icon) and are stored locally.

## Requirements

- [Node.js](https://nodejs.org) 22+ and npm
- [Rust](https://www.rust-lang.org/tools/install) and the Tauri prerequisites for your platform
- A GitHub token: `GITHUB_TOKEN`, a signed-in Copilot editor, or the [GitHub CLI](https://cli.github.com) logged in (`gh auth login`)

## Quick start

```
git clone https://github.com/wouter-actemium/copilot-usage-overlay.git
cd copilot-usage-overlay
npm install
npm start
```

On Windows you can also double-click `run.cmd`.

## Configuration

Secrets are read from environment variables or a `.env` file in the working directory. `.env` is git-ignored; never commit it.

| Variable | Purpose |
| --- | --- |
| `GITHUB_TOKEN` | Token for the Copilot usage API (falls back to the editor's Copilot login, then `gh auth token`) |
| `AZURE_DEVOPS_ORG_URL` | Azure DevOps organisation URL, enables the Azure DevOps pull request source |
| `AZURE_DEVOPS_PAT` | Personal access token for Azure DevOps |
| `AZURE_DEVOPS_PROJECTS` | Optional list of projects to limit the search to |
| `TRM_PATH` | Optional path of the app opened by the TRM reminder |

Task sources can be switched off individually in the settings.

## Data storage

The SQLite database (`usage.db`) is **not** part of this repo. It is created automatically on first launch in Tauri's app data folder and history builds up from then on.

### Why a local database?

GitHub does not expose historical usage to regular (non-admin) users. The API only returns your current total, so without admin rights there is no way to look up past data. The app therefore records every change it sees in `usage.db`, and history can only be built from the moments the app is running.

**Recommendation:** start the app automatically at login (e.g. build the Windows installer with `npm run build` and put a shortcut to it in `shell:startup` (Win+R, type `shell:startup`)). The more it runs, the more complete your history is; usage while the app is closed is only seen as one combined change the next time it starts.

## Build a distributable

```
npm run build
```

Output goes to `src-tauri/target/release/bundle/` (installer on Windows, dmg on macOS, AppImage on Linux).
