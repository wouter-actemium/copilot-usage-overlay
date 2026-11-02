//! Generic task list fed by pluggable sources (pull requests, notifications, ...).
//! Add a source by implementing [`TaskSource`] and listing it in [`sources`].

use reqwest::{blocking::Client, Url};
use rusqlite::{params, Connection};
use crate::reminder::{Reminder, TrmReminder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, env};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY, source TEXT NOT NULL, badge TEXT NOT NULL, title TEXT NOT NULL,
    subtitle TEXT NOT NULL, url TEXT NOT NULL, added INTEGER NOT NULL, dismissed INTEGER NOT NULL DEFAULT 0);";

#[derive(Serialize, Clone)]
pub struct Task {
    /// Globally unique: `<source>:<external id>`.
    pub id: String,
    pub source: String,
    /// Short label shown in the UI (e.g. "GH").
    pub badge: String,
    pub title: String,
    pub subtitle: String,
    pub url: String,
}

#[derive(Serialize)]
pub struct SyncResult {
    pub tasks: Vec<Task>,
    pub errors: Vec<String>,
}

pub trait TaskSource {
    fn id(&self) -> &'static str;
    /// `Ok(None)` means the source is not configured and is skipped.
    fn fetch(&self, client: &Client) -> Result<Option<Vec<Task>>, String>;
}

fn sources(reminder: &Reminder) -> Vec<Box<dyn TaskSource>> {
    vec![
        Box::new(GitHubPullRequests),
        Box::new(AzureDevOpsPullRequests),
        Box::new(crate::teams::TeamsNotifications),
        Box::new(TrmReminder(reminder.clone())),
    ]
}

fn env_var(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

fn get_json(request: reqwest::blocking::RequestBuilder) -> Result<Value, String> {
    request.send().map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?
        .json().map_err(|e| format!("unexpected response ({e})"))
}

struct GitHubPullRequests;

impl GitHubPullRequests {
    fn task(item: &Value) -> Option<Task> {
        let repo = item["repository_url"].as_str()?.split("/repos/").nth(1)?;
        Some(Task {
            id: format!("github:{}", item["id"].as_i64()?),
            source: "github".into(),
            badge: "GH".into(),
            title: item["title"].as_str()?.to_owned(),
            subtitle: format!("{repo}#{} · {}", item["number"].as_i64()?, item["user"]["login"].as_str().unwrap_or("?")),
            url: item["html_url"].as_str()?.to_owned(),
        })
    }
}

impl TaskSource for GitHubPullRequests {
    fn id(&self) -> &'static str { "github" }

    fn fetch(&self, client: &Client) -> Result<Option<Vec<Task>>, String> {
        let token = crate::token()?;
        let mut seen = HashSet::new();
        let mut tasks = Vec::new();
        for qualifier in ["review-requested:@me", "assignee:@me"] {
            let query = format!("is:pr is:open archived:false draft:false {qualifier}");
            let response = get_json(client.get("https://api.github.com/search/issues").bearer_auth(&token)
                .header("Accept", "application/vnd.github+json").header("User-Agent", "copilot-overlay")
                .query(&[("q", query.as_str()), ("per_page", "30")]))?;
            for task in response["items"].as_array().into_iter().flatten().filter_map(Self::task) {
                if seen.insert(task.id.clone()) { tasks.push(task); }
            }
        }
        Ok(Some(tasks))
    }
}

/// Needs `AZURE_DEVOPS_ORG_URL` and `AZURE_DEVOPS_PAT` (scope: Code - Read);
/// `AZURE_DEVOPS_PROJECTS` (comma separated) optionally limits the projects.
struct AzureDevOpsPullRequests;

fn url_with(base: &str, segments: &[&str]) -> Result<Url, String> {
    let mut url = Url::parse(base).map_err(|e| e.to_string())?;
    url.path_segments_mut().map_err(|_| "invalid AZURE_DEVOPS_ORG_URL")?.pop_if_empty().extend(segments);
    Ok(url)
}

impl TaskSource for AzureDevOpsPullRequests {
    fn id(&self) -> &'static str { "azuredevops" }

    fn fetch(&self, client: &Client) -> Result<Option<Vec<Task>>, String> {
        let (Some(org), Some(pat)) = (env_var("AZURE_DEVOPS_ORG_URL"), env_var("AZURE_DEVOPS_PAT")) else { return Ok(None) };
        // The PAT is sent as basic auth, so never over plain http.
        if !org.starts_with("https://") { return Err("AZURE_DEVOPS_ORG_URL must start with https://".into()); }
        let get = |mut url: Url, query: &[(&str, &str)]| {
            url.query_pairs_mut().extend_pairs(query);
            get_json(client.get(url).basic_auth("", Some(&pat)).header("Accept", "application/json"))
                .map_err(|e| format!("Azure DevOps: {e}"))
        };

        let me = get(url_with(&org, &["_apis", "connectionData"])?, &[("api-version", "7.1-preview.1")])?["authenticatedUser"]["id"]
            .as_str().ok_or("Azure DevOps: could not determine the current user")?.to_owned();
        let projects: Vec<String> = match env_var("AZURE_DEVOPS_PROJECTS") {
            Some(list) => list.split(',').map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect(),
            None => get(url_with(&org, &["_apis", "projects"])?, &[("api-version", "7.1")])?["value"].as_array().into_iter().flatten()
                .filter_map(|p| p["name"].as_str().map(str::to_owned)).collect(),
        };

        let mut tasks = Vec::new();
        for project in projects {
            let response = get(url_with(&org, &[&project, "_apis", "git", "pullrequests"])?,
                &[("searchCriteria.status", "active"), ("searchCriteria.reviewerId", &me), ("$top", "50"), ("api-version", "7.1")])?;
            for pr in response["value"].as_array().into_iter().flatten() {
                let my_vote = pr["reviewers"].as_array().into_iter().flatten().find(|r| r["id"].as_str() == Some(&me)).and_then(|r| r["vote"].as_i64());
                if pr["isDraft"].as_bool() == Some(true) || my_vote.unwrap_or(0) != 0 { continue; }
                let (Some(number), Some(title), Some(repo)) = (pr["pullRequestId"].as_i64(), pr["title"].as_str(), pr["repository"]["name"].as_str()) else { continue };
                tasks.push(Task {
                    id: format!("azuredevops:{number}"),
                    source: "azuredevops".into(),
                    badge: "ADO".into(),
                    title: title.to_owned(),
                    subtitle: format!("{repo}!{number} · {}", pr["createdBy"]["displayName"].as_str().unwrap_or("?")),
                    url: url_with(&org, &[&project, "_git", repo, "pullrequest", &number.to_string()])?.into(),
                });
            }
        }
        Ok(Some(tasks))
    }
}

/// Upserts the fetched tasks (keeping their dismissed flag) and drops rows the source no longer reports.
fn store(conn: &mut Connection, source: &str, fetched: &[Task], now: i64) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    for task in fetched {
        tx.execute(
            "INSERT INTO tasks (id,source,badge,title,subtitle,url,added) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(id) DO UPDATE SET title=excluded.title, subtitle=excluded.subtitle, url=excluded.url",
            params![task.id, task.source, task.badge, task.title, task.subtitle, task.url, now],
        ).map_err(|e| e.to_string())?;
    }
    let keep: HashSet<&str> = fetched.iter().map(|t| t.id.as_str()).collect();
    let existing = tx.prepare("SELECT id FROM tasks WHERE source=?1").map_err(|e| e.to_string())?
        .query_map(params![source], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    for id in existing.iter().filter(|id| !keep.contains(id.as_str())) {
        tx.execute("DELETE FROM tasks WHERE id=?1", params![id]).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

pub fn list(conn: &Connection) -> Result<Vec<Task>, String> {
    conn.prepare("SELECT id,source,badge,title,subtitle,url FROM tasks WHERE dismissed=0 ORDER BY added DESC, id").map_err(|e| e.to_string())?
        .query_map([], |r| Ok(Task { id: r.get(0)?, source: r.get(1)?, badge: r.get(2)?, title: r.get(3)?, subtitle: r.get(4)?, url: r.get(5)? }))
        .map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

pub fn dismiss(conn: &Connection, id: &str) -> Result<(), String> {
    conn.execute("UPDATE tasks SET dismissed=1 WHERE id=?1", params![id]).map(|_| ()).map_err(|e| e.to_string())
}

pub fn url(conn: &Connection, id: &str) -> Result<String, String> {
    conn.query_row("SELECT url FROM tasks WHERE id=?1", params![id], |r| r.get(0)).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
pub struct SyncOptions {
    /// Sources that are skipped and whose stored tasks are removed.
    pub disabled: Vec<String>,
    pub reminder: Reminder,
    /// When not empty, only these sources are synced.
    #[serde(default)]
    pub only: Vec<String>,
}

pub fn sync(conn: &mut Connection, now: i64, options: &SyncOptions) -> SyncResult {
    let client = Client::new();
    let mut errors = Vec::new();
    for source in sources(&options.reminder) {
        if !options.only.is_empty() && !options.only.iter().any(|o| o == source.id()) { continue; }
        if options.disabled.iter().any(|d| d == source.id()) {
            if let Err(e) = store(conn, source.id(), &[], now) { errors.push(format!("{}: {e}", source.id())); }
            continue;
        }
        match source.fetch(&client) {
            Ok(Some(fetched)) => if let Err(e) = store(conn, source.id(), &fetched, now) { errors.push(format!("{}: {e}", source.id())); },
            Ok(None) => {}
            Err(e) => errors.push(format!("{}: {e}", source.id())),
        }
    }
    match list(conn) {
        Ok(tasks) => SyncResult { tasks, errors },
        Err(e) => { errors.push(e); SyncResult { tasks: Vec::new(), errors } }
    }
}
