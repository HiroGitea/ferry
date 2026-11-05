//! Notifications to users: a webhook (ferry JSON or a chat bot's incoming-webhook format) and/or
//! Telegram. Job completions are picked up from the event bus; other parts of the daemon (idle
//! reclaim, preemption) call [`send`] directly.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use ferry_core::api::{Event, NotifySettings, WebhookFormat};
use ferry_core::{Job, JobState, UserId};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A job or task step reached a terminal state.
    Finished,
    /// The job's GPUs have been idle; it will be cancelled unless they get busy again.
    IdleWarning,
    IdleCancelled,
    /// An admin preempted the job; it went back to the queue.
    Preempted,
    Test,
}

impl Kind {
    fn as_str(&self) -> &'static str {
        match self {
            Kind::Finished => "job.finished",
            Kind::IdleWarning => "job.idle_warning",
            Kind::IdleCancelled => "job.idle_cancelled",
            Kind::Preempted => "job.preempted",
            Kind::Test => "test",
        }
    }
}

pub struct Notification {
    pub kind: Kind,
    /// One line, used as the message title.
    pub title: String,
    /// Extra lines.
    pub body: String,
    pub job: Option<Job>,
}

impl Notification {
    pub fn for_job(kind: Kind, job: &Job, detail: impl Into<String>) -> Self {
        let what = if let Some(t) = job.task_id {
            format!("task {t} step {}", job.id)
        } else {
            format!("job {}", job.id)
        };
        let title = match kind {
            Kind::Finished => format!(
                "{} {what} ({}) {}",
                state_icon(job.state),
                job.name,
                job.state.as_str()
            ),
            Kind::IdleWarning => format!("⚠️ {what} ({}): GPUs idle", job.name),
            Kind::IdleCancelled => format!("🛑 {what} ({}) cancelled: GPUs idle", job.name),
            Kind::Preempted => format!("⏸️ {what} ({}) preempted and requeued", job.name),
            Kind::Test => "ferry test notification".into(),
        };
        let mut lines = vec![];
        let detail = detail.into();
        if !detail.is_empty() {
            lines.push(detail);
        }
        if let Some(code) = job.exit_code
            && kind == Kind::Finished
        {
            lines.push(format!("exit code {code}"));
        }
        if let (Some(s), Some(f)) = (job.started_at, job.finished_at) {
            lines.push(format!(
                "runtime {}",
                fmt_secs((f - s).num_seconds().max(0))
            ));
        }
        if let Some(n) = &job.node_id {
            lines.push(format!("node {n}"));
        }
        if let Some(r) = &job.reason
            && kind == Kind::Finished
            && job.state != JobState::Succeeded
            && !r.starts_with("exit code")
        {
            lines.push(r.clone());
        }
        Notification {
            kind,
            title,
            body: lines.join("\n"),
            job: Some(job.clone()),
        }
    }
}

fn state_icon(s: JobState) -> &'static str {
    match s {
        JobState::Succeeded => "✅",
        JobState::Failed => "❌",
        JobState::Timeout => "⏱️",
        JobState::Cancelled => "🚫",
        _ => "ℹ️",
    }
}

fn fmt_secs(s: i64) -> String {
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

fn link(public_url: Option<&str>, n: &Notification) -> Option<String> {
    let base = public_url?.trim_end_matches('/');
    let job = n.job.as_ref()?;
    Some(match job.task_id {
        Some(t) => format!("{base}/tasks/{t}"),
        None => format!("{base}/jobs?job={}", job.id),
    })
}

fn text(n: &Notification, link: Option<&str>) -> String {
    let mut t = n.title.clone();
    if !n.body.is_empty() {
        t.push('\n');
        t.push_str(&n.body);
    }
    if let Some(l) = link {
        t.push('\n');
        t.push_str(l);
    }
    t
}

fn webhook_body(fmt: WebhookFormat, n: &Notification, link: Option<&str>) -> serde_json::Value {
    let text = text(n, link);
    match fmt {
        WebhookFormat::Json => json!({
            "event": n.kind.as_str(),
            "title": n.title,
            "text": text,
            "job": n.job,
            "url": link,
        }),
        WebhookFormat::Slack => json!({ "text": text }),
        WebhookFormat::Feishu => json!({ "msg_type": "text", "content": { "text": text } }),
        WebhookFormat::Dingtalk | WebhookFormat::Wecom => {
            json!({ "msgtype": "text", "text": { "content": text } })
        }
    }
}

/// Deliver to every channel the user configured. Returns one error string per failed channel.
pub async fn deliver(state: &AppState, settings: &NotifySettings, n: &Notification) -> Vec<String> {
    let client = http();
    let mut errors = vec![];
    let link = link(state.config.notify.public_url.as_deref(), n);
    let link = link.as_deref();
    if let Some(url) = settings.webhook_url.as_deref().filter(|u| !u.is_empty()) {
        let body = webhook_body(settings.webhook_format, n, link);
        match client.post(url).json(&body).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => errors.push(format!("webhook: HTTP {}", r.status())),
            Err(e) => errors.push(format!("webhook: {e}")),
        }
    }
    if let Some(chat) = settings
        .telegram_chat_id
        .as_deref()
        .filter(|c| !c.is_empty())
    {
        match state.config.notify.telegram_bot_token.as_deref() {
            None => errors.push("telegram: no bot configured on the server".into()),
            Some(token) => {
                let api = state
                    .config
                    .notify
                    .telegram_api
                    .as_deref()
                    .unwrap_or("https://api.telegram.org")
                    .trim_end_matches('/');
                let r = client
                    .post(format!("{api}/bot{token}/sendMessage"))
                    .json(&json!({
                        "chat_id": chat,
                        "text": text(n, link),
                        "disable_web_page_preview": true,
                    }))
                    .send()
                    .await;
                match r {
                    Ok(r) if r.status().is_success() => {}
                    Ok(r) => {
                        let status = r.status();
                        let detail = r
                            .json::<serde_json::Value>()
                            .await
                            .ok()
                            .and_then(|v| v["description"].as_str().map(str::to_string))
                            .unwrap_or_default();
                        errors.push(format!("telegram: HTTP {status} {detail}"));
                    }
                    // Never echo the URL: it contains the bot token.
                    Err(e) => errors.push(format!("telegram: {}", e.without_url())),
                }
            }
        }
    }
    errors
}

/// Fire-and-forget delivery to a user (always sent, regardless of the on_* preferences).
pub fn send(state: Arc<AppState>, user_id: UserId, n: Notification) {
    tokio::spawn(async move {
        let settings = match state.store.notify_settings(user_id).await {
            Ok(s) => s,
            Err(e) => return warn!(error = %e, "loading notification settings"),
        };
        for e in deliver(&state, &settings, &n).await {
            warn!(user_id, error = %e, kind = n.kind.as_str(), "notification failed");
        }
    });
}

fn wanted(s: &NotifySettings, job: &Job) -> bool {
    if job.task_id.is_some() && !s.task_steps {
        return false;
    }
    match job.state {
        JobState::Succeeded => s.on_success,
        JobState::Failed | JobState::Timeout => s.on_failure,
        // The user cancelled it themselves; admin/idle cancellations are sent explicitly.
        _ => false,
    }
}

/// Watch the event bus and notify owners when their jobs finish.
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut rx = state.events.subscribe();
        // A finished job can be published more than once (archive, cleanup): notify once.
        let mut seen: HashSet<i64> = HashSet::new();
        let mut order: VecDeque<i64> = VecDeque::new();
        loop {
            let job = match rx.recv().await {
                Ok(Event::JobUpdated { job }) if job.state.is_terminal() => job,
                Ok(_) => continue,
                Err(RecvError::Lagged(n)) => {
                    debug!(skipped = n, "notifier lagged");
                    continue;
                }
                Err(RecvError::Closed) => return,
            };
            if !seen.insert(job.id) {
                continue;
            }
            order.push_back(job.id);
            if order.len() > 10_000
                && let Some(old) = order.pop_front()
            {
                seen.remove(&old);
            }
            let st = state.clone();
            tokio::spawn(async move {
                let Ok(settings) = st.store.notify_settings(job.user_id).await else {
                    return;
                };
                if settings.webhook_url.is_none() && settings.telegram_chat_id.is_none() {
                    return;
                }
                if !wanted(&settings, &job) {
                    return;
                }
                let n = Notification::for_job(Kind::Finished, &job, String::new());
                for e in deliver(&st, &settings, &n).await {
                    warn!(job = job.id, error = %e, "notification failed");
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_match_bot_schemas() {
        let n = Notification {
            kind: Kind::Test,
            title: "t".into(),
            body: "b".into(),
            job: None,
        };
        let f = |fmt| webhook_body(fmt, &n, Some("http://x/tasks/1"));
        assert_eq!(f(WebhookFormat::Slack)["text"], "t\nb\nhttp://x/tasks/1");
        assert_eq!(f(WebhookFormat::Feishu)["msg_type"], "text");
        assert_eq!(
            f(WebhookFormat::Feishu)["content"]["text"],
            "t\nb\nhttp://x/tasks/1"
        );
        assert_eq!(f(WebhookFormat::Dingtalk)["msgtype"], "text");
        assert_eq!(
            f(WebhookFormat::Wecom)["text"]["content"],
            "t\nb\nhttp://x/tasks/1"
        );
        let j = f(WebhookFormat::Json);
        assert_eq!(
            (j["event"].as_str(), j["url"].as_str()),
            (Some("test"), Some("http://x/tasks/1"))
        );
        assert_eq!(link(None, &n), None);
    }
}
