# Copilot Overlay

Always-on-top Electron overlay that shows your GitHub Copilot premium request usage.

## Requirements

- [Node.js](https://nodejs.org) 22+ and npm
- Either the [GitHub CLI](https://cli.github.com) logged in (`gh auth login`), or a `GITHUB_TOKEN`

## Quick start

```
git clone <repo-url>
cd copilot_usage
npm install
npm start
```

## Authentication

By default the app uses `gh auth token`. To use a token instead, copy `.env.example` to `.env` and set `GITHUB_TOKEN`. `.env` is git-ignored; never commit it.

## Data storage

The SQLite database (`usage.db`) is **not** part of this repo. It is created automatically on first launch in Electron's user data folder (on Windows `%APPDATA%\copilot-overlay`) and history builds up from then on.

### Why a local database?

GitHub does not expose historical usage to regular (non-admin) users. The API only returns your current total, so without admin rights there is no way to look up past data. The app therefore records every change it sees in `usage.db`, and history can only be built from the moments the app is running.

**Recommendation:** start the app automatically at login (e.g. build the portable exe with `npm run dist` and put a shortcut to it in `shell:startup` (Win+R, type `shell:startup`)). The more it runs, the more complete your history is; usage while the app is closed is only seen as one combined change the next time it starts.

## Build a distributable

```
npm run dist
```

Output goes to `dist/` (portable exe on Windows, dmg on macOS, AppImage on Linux).
