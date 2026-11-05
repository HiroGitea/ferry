//! Interactive shells into running jobs and task environments (`ferry shell`).
//!
//! Wire protocol on the WebSocket: binary frames carry raw terminal bytes in both directions;
//! text frames are JSON control messages: client → `{"resize":[cols,rows]}`, server →
//! `{"exit":code}` right before closing. The control daemon either runs the command under a
//! PTY itself (local node) or proxies the socket to the agent that owns the container.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// What to run under the PTY.
#[derive(Debug, Clone)]
pub struct ShellTarget {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// Query of the shell endpoints (control API and agent).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellQuery {
    /// JSON array; default: a login shell (bash if present, else sh).
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

impl ShellQuery {
    pub fn command(&self) -> anyhow::Result<Vec<String>> {
        match self.cmd.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => {
                let v: Vec<String> = serde_json::from_str(c)?;
                anyhow::ensure!(!v.is_empty(), "empty command");
                Ok(v)
            }
            None => Ok(default_shell()),
        }
    }
}

pub fn default_shell() -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        "if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh -l; fi".into(),
    ]
}

#[derive(Deserialize)]
struct Control {
    resize: Option<(u16, u16)>,
}

fn set_size(fd: i32, cols: u16, rows: u16) {
    let ws = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is an open PTY master; TIOCSWINSZ only reads `ws`.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

fn open_pty(cols: u16, rows: u16) -> anyhow::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let ws = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two fds; name and termios are optional (null).
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    anyhow::ensure!(r == 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both fds were just returned by openpty and are owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

/// Run `target` under a fresh PTY and connect it to the WebSocket until either side ends.
pub async fn bridge_pty(socket: WebSocket, target: ShellTarget, cols: u16, rows: u16) {
    let (mut tx, mut rx) = socket.split();
    let fail =
        |e: anyhow::Error| Message::Text(format!("{{\"error\":{:?}}}", format!("{e:#}")).into());
    let (master, slave) = match open_pty(cols, rows) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(fail(e)).await;
            return;
        }
    };
    let mut cmd = tokio::process::Command::new(&target.program);
    cmd.args(&target.args)
        .env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        )
        .kill_on_drop(true);
    if let Some(d) = &target.cwd {
        cmd.current_dir(d);
    }
    let (Ok(si), Ok(so), Ok(se)) = (slave.try_clone(), slave.try_clone(), slave.try_clone()) else {
        let _ = tx.send(fail(anyhow::anyhow!("dup pty"))).await;
        return;
    };
    cmd.stdin(si).stdout(so).stderr(se);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(fail(anyhow::anyhow!("starting {}: {e}", target.program)))
                .await;
            return;
        }
    };
    // The command keeps copies of the slave fds; without dropping them the master never sees EOF.
    drop(cmd);
    drop(slave);
    let master_fd = master.as_raw_fd();
    let (Ok(read_fd), Ok(write_fd)) = (master.try_clone(), master.try_clone()) else {
        return;
    };

    // PTY -> socket (blocking reads on a thread; ends with EIO when the child exits).
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut f = std::fs::File::from(read_fd);
        let mut buf = [0u8; 8192];
        loop {
            match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    // socket -> PTY
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut f = std::fs::File::from(write_fd);
        while let Ok(b) = in_rx.recv() {
            if f.write_all(&b).is_err() {
                break;
            }
        }
    });

    let mut exited: Option<i32> = None;
    loop {
        // Once the child is gone, drain what is left and stop (a grandchild may keep the PTY open).
        let drain = async {
            if exited.is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await
            } else {
                std::future::pending::<()>().await
            }
        };
        tokio::select! {
            _ = drain => break,
            out = out_rx.recv() => match out {
                Some(b) => {
                    if tx.send(Message::Binary(b.into())).await.is_err() {
                        break;
                    }
                }
                // Reader ended: the child closed the terminal (exited).
                None => break,
            },
            msg = rx.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    let _ = in_tx.send(b.to_vec());
                }
                Some(Ok(Message::Text(t))) => {
                    if let Ok(Control { resize: Some((c, r)) }) = serde_json::from_str::<Control>(&t) {
                        set_size(master_fd, c, r);
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    debug!("shell client went away");
                    let _ = child.start_kill();
                    break;
                }
                _ => {}
            },
            st = child.wait(), if exited.is_none() => {
                exited = Some(st.ok().and_then(|s| s.code()).unwrap_or(-1));
            }
        }
    }
    let code = match exited {
        Some(c) => c,
        None => match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(Ok(s)) => s.code().unwrap_or(-1),
            _ => {
                let _ = child.start_kill();
                -1
            }
        },
    };
    // Flush whatever output is still queued.
    while let Ok(b) = out_rx.try_recv() {
        let _ = tx.send(Message::Binary(b.into())).await;
    }
    drop(master);
    let _ = tx
        .send(Message::Text(format!("{{\"exit\":{code}}}").into()))
        .await;
    let _ = tx.send(Message::Close(None)).await;
}

/// Control node → agent: forward the client's socket to the agent's `/shell` endpoint.
pub async fn proxy_to_agent(socket: WebSocket, url: String, token: String) {
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return warn!("bad agent shell url: {e}"),
    };
    if let Ok(v) = format!("Bearer {token}").parse() {
        req.headers_mut().insert("authorization", v);
    }
    let (mut client_tx, mut client_rx) = socket.split();
    let agent = match tokio_tungstenite::connect_async(req).await {
        Ok((s, _)) => s,
        Err(e) => {
            let _ = client_tx
                .send(Message::Text(
                    format!("{{\"error\":\"agent: {e}\"}}").into(),
                ))
                .await;
            return;
        }
    };
    let (mut agent_tx, mut agent_rx) = agent.split();
    loop {
        tokio::select! {
            m = client_rx.next() => match m {
                Some(Ok(Message::Binary(b))) => { if agent_tx.send(tungstenite::Message::Binary(b)).await.is_err() { break } }
                Some(Ok(Message::Text(t))) => { if agent_tx.send(tungstenite::Message::Text(t.as_str().into())).await.is_err() { break } }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => { let _ = agent_tx.send(tungstenite::Message::Close(None)).await; break }
                _ => {}
            },
            m = agent_rx.next() => match m {
                Some(Ok(tungstenite::Message::Binary(b))) => { if client_tx.send(Message::Binary(b)).await.is_err() { break } }
                Some(Ok(tungstenite::Message::Text(t))) => { if client_tx.send(Message::Text(t.as_str().into())).await.is_err() { break } }
                Some(Ok(tungstenite::Message::Close(_))) | None | Some(Err(_)) => { let _ = client_tx.send(Message::Close(None)).await; break }
                _ => {}
            },
        }
    }
}
