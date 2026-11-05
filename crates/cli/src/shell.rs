//! `ferry shell`: an interactive terminal inside a running job, or a task environment.

use std::io::{IsTerminal, Read, Write};
use std::time::Duration;

use anyhow::{Context, bail};
use ferry_client::Client;
use ferry_core::api::SubmitJobRequest;
use ferry_core::{Job, JobId, JobState};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// The holder step of a task shell session keeps the environment alive until this file (in
/// /work, the step's working directory) is removed; then it exits 0 and becomes a snapshot.
const HOLD_FILE: &str = ".ferry-shell";

struct RawMode(bool);
impl RawMode {
    fn enable() -> Self {
        let on = std::io::stdin().is_terminal() && crossterm::terminal::enable_raw_mode().is_ok();
        RawMode(on)
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// One reader of stdin for the whole process, so the terminal session and the prompt after it
/// do not race for input (a blocking read cannot be cancelled).
fn stdin_pump() -> &'static tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Vec<u8>>> {
    static PUMP: std::sync::OnceLock<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Vec<u8>>>> =
        std::sync::OnceLock::new();
    PUMP.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        tokio::sync::Mutex::new(rx)
    })
}

fn term_size() -> (u16, u16) {
    match crossterm::terminal::size() {
        Ok((c, r)) if c > 0 && r > 0 => (c, r),
        _ => (80, 24),
    }
}

/// Connect to a job's shell endpoint and run until the remote side exits. Returns its exit code.
pub async fn attach(
    c: &Client,
    job: JobId,
    cmd: Option<&[String]>,
    interactive: bool,
) -> anyhow::Result<i32> {
    let (cols, rows) = term_size();
    let base = c.server_url();
    let ws_base = if let Some(r) = base.strip_prefix("https://") {
        format!("wss://{r}")
    } else {
        format!("ws://{}", base.trim_start_matches("http://"))
    };
    let mut q = vec![("cols", cols.to_string()), ("rows", rows.to_string())];
    if let Some(cmd) = cmd {
        q.push(("cmd", serde_json::to_string(cmd)?));
    }
    let query: String = q
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut req = format!("{ws_base}/api/v1/jobs/{job}/shell?{query}").into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {}", c.token()).parse()?);
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| match e {
            tokio_tungstenite::tungstenite::Error::Http(r) => {
                let body = r
                    .body()
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .unwrap_or_default();
                let msg = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["error"].as_str().map(str::to_string))
                    .unwrap_or(body);
                anyhow::anyhow!("{}: {msg}", r.status())
            }
            e => anyhow::anyhow!("connecting to {ws_base}: {e}"),
        })?;
    let (mut tx, mut rx) = ws.split();

    let _raw = if interactive {
        Some(RawMode::enable())
    } else {
        None
    };
    let mut in_rx = stdin_pump().lock().await;
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdout = std::io::stdout();
    let mut code = -1;
    let mut stdin_open = interactive;
    loop {
        tokio::select! {
            b = in_rx.recv(), if stdin_open => match b {
                Some(b) => { tx.send(Message::Binary(b.into())).await?; }
                // EOF on a pipe: Ctrl-D for the remote side.
                None => {
                    tx.send(Message::Binary(vec![4].into())).await?;
                    stdin_open = false;
                }
            },
            _ = winch.recv() => {
                let (c, r) = term_size();
                tx.send(Message::Text(format!("{{\"resize\":[{c},{r}]}}").into())).await?;
            }
            m = rx.next() => match m {
                Some(Ok(Message::Binary(b))) => {
                    stdout.write_all(&b)?;
                    stdout.flush()?;
                }
                Some(Ok(Message::Text(t))) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                    if let Some(c) = v["exit"].as_i64() {
                        code = c as i32;
                    }
                    if let Some(e) = v["error"].as_str() {
                        drop(_raw);
                        bail!("{e}");
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(e)) => {
                    drop(_raw);
                    bail!("connection lost: {e}");
                }
                _ => {}
            },
        }
    }
    Ok(code)
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn wait_running(c: &Client, id: JobId) -> anyhow::Result<Job> {
    let mut said = false;
    loop {
        let j = c.job(id).await?;
        match j.state {
            JobState::Running if j.container_id.is_some() => return Ok(j),
            s if s.is_terminal() => bail!(
                "step {id} ended before the shell could start: {s} {}",
                j.reason.unwrap_or_default()
            ),
            _ => {
                if !said && j.state == JobState::Queued {
                    eprintln!(
                        "waiting for resources ({})…",
                        j.reason.as_deref().unwrap_or("queued")
                    );
                    said = true;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

async fn ask_yes(prompt: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return true;
    }
    eprint!("{prompt} [Y/n] ");
    let _ = std::io::stderr().flush();
    let mut rx = stdin_pump().lock().await;
    let mut line = Vec::new();
    while let Some(b) = rx.recv().await {
        line.extend_from_slice(&b);
        if line.contains(&b'\n') || line.contains(&b'\r') {
            break;
        }
    }
    let s = String::from_utf8_lossy(&line).trim().to_ascii_lowercase();
    !matches!(s.as_str(), "n" | "no")
}

/// Shell into a task's environment. Attaches to a running step if there is one; otherwise runs a
/// "shell" step that holds the environment, and on exit saves it as a snapshot (or rolls back).
pub async fn task_shell(
    c: &Client,
    task: i64,
    cmd: Option<&[String]>,
    time_limit_secs: u64,
) -> anyhow::Result<i32> {
    let d = c.task(task).await?;
    if let Some(running) = d.steps.iter().find(|j| j.state == JobState::Running) {
        eprintln!(
            "attaching to running step {} ({}); changes are saved with that step",
            running.id, running.name
        );
        return attach(c, running.id, cmd, true).await;
    }
    let hold = format!("touch {HOLD_FILE}; while [ -e {HOLD_FILE} ]; do sleep 1; done");
    let req = SubmitJobRequest {
        name: Some("shell".into()),
        command: vec!["sh".into(), "-c".into(), hold],
        env: [("FERRY_INTERACTIVE".to_string(), "1".to_string())].into(),
        time_limit_secs: Some(time_limit_secs),
        task_id: Some(task),
        ..Default::default()
    };
    let r = c.submit(&req).await?;
    let step = r.jobs.first().context("no step created")?.id;
    eprintln!(
        "shell step {step} in task {task}: exit the shell to save the environment as a snapshot"
    );
    let job = wait_running(c, step).await?;
    let code = attach(c, job.id, cmd, true).await;
    // Whatever happened in the shell, decide what to do with the environment.
    let keep = c
        .job(step)
        .await
        .map(|j| j.state == JobState::Running)
        .unwrap_or(false);
    if !keep {
        eprintln!("step {step} already ended");
        return code;
    }
    if ask_yes("Save the environment as a snapshot?").await {
        let rm = vec!["rm".to_string(), "-f".into(), HOLD_FILE.into()];
        attach(c, step, Some(&rm), false)
            .await
            .context("releasing the shell step")?;
        let j = c.wait(step, 600).await?;
        match (j.state, j.snapshot_id) {
            (JobState::Succeeded, Some(s)) => eprintln!("saved as snapshot {s} (task {task} HEAD)"),
            (JobState::Succeeded, None) => {
                eprintln!("step {step} finished (this runtime keeps no snapshots)")
            }
            (s, _) => eprintln!("step {step} ended {s}: {}", j.reason.unwrap_or_default()),
        }
    } else {
        c.cancel(step).await?;
        eprintln!("discarded: task {task} stays at its previous snapshot");
    }
    code
}
