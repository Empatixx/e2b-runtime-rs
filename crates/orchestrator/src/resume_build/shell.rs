use std::collections::BTreeMap;
use std::io::{Read, Write};

use rustix::termios::{OptionalActions, Termios, isatty, tcgetattr, tcgetwinsize, tcsetattr};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::guest::Guest;
use crate::envd::pty::{PtyEvent, PtySize, PtyTarget};

const DEFAULT_SIZE: PtySize = PtySize { cols: 80, rows: 24 };
const DEFAULT_TERM: &str = "xterm-256color";
const LOCALE_VARS: [&str; 4] = ["LANG", "LC_ALL", "LC_CTYPE", "COLORTERM"];
const INPUT_CHUNK: usize = 4096;

pub const SHELL_EXITED_PREFIX: &str = "shell exited with code";

pub fn is_shell_exited(error: &str) -> bool {
    error.starts_with(SHELL_EXITED_PREFIX)
}

pub async fn attach(guest: &Guest, cancel: &CancellationToken) -> Result<(), String> {
    match session(guest, cancel).await {
        Err(error) if is_shell_exited(&error) => Ok(()),
        result => result,
    }
}

async fn session(guest: &Guest, cancel: &CancellationToken) -> Result<(), String> {
    if !isatty(std::io::stdin()) {
        return Err("-shell requires an interactive terminal on stdin".to_owned());
    }
    let target = PtyTarget {
        envd: guest.envd,
        access_token: Some(guest.access_token.clone()),
        user: "root".to_owned(),
    };
    let mut events = target.start_shell(&shell_env(), terminal_size()).await?;
    let pid = wait_for_start(&mut events).await?;
    println!("📟 Attaching shell via envd (Ctrl+D to exit)");
    let raw = RawTerminal::enter()?;
    let session = CancellationToken::new();
    let input = tokio::task::spawn_blocking({
        let (target, session) = (target.clone(), session.clone());
        let runtime = tokio::runtime::Handle::current();
        move || pump_input(&target, pid, &session, &runtime)
    });
    let resize = tokio::spawn(pump_resize(target.clone(), pid, session.clone()));
    let ended = tokio::select! {
        ended = pump_output(&mut events) => ended,
        () = cancel.cancelled() => Ok(None),
    };
    session.cancel();
    resize.abort();
    drop(raw);
    drop(input);
    println!();
    match ended? {
        Some(code) => Err(format!("{SHELL_EXITED_PREFIX} {code}")),
        None => Ok(()),
    }
}

fn shell_env() -> BTreeMap<String, String> {
    let term = std::env::var("TERM")
        .ok()
        .filter(|term| !term.is_empty())
        .unwrap_or_else(|| DEFAULT_TERM.to_owned());
    LOCALE_VARS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_owned(), value))
        })
        .chain([("TERM".to_owned(), term)])
        .collect()
}

fn terminal_size() -> PtySize {
    tcgetwinsize(std::io::stdout())
        .ok()
        .filter(|size| size.ws_col > 0 && size.ws_row > 0)
        .map_or(DEFAULT_SIZE, |size| PtySize {
            cols: size.ws_col,
            rows: size.ws_row,
        })
}

async fn wait_for_start(events: &mut mpsc::Receiver<Result<PtyEvent, String>>) -> Result<u32, String> {
    while let Some(event) = events.recv().await {
        match event.map_err(|e| format!("stream closed before start: {e}"))? {
            PtyEvent::Started(pid) => return Ok(pid),
            PtyEvent::Output(bytes) => write_stdout(&bytes),
            PtyEvent::Ended(code) => return Err(format!("{SHELL_EXITED_PREFIX} {code}")),
        }
    }
    Err("no start event received".to_owned())
}

async fn pump_output(events: &mut mpsc::Receiver<Result<PtyEvent, String>>) -> Result<Option<i32>, String> {
    while let Some(event) = events.recv().await {
        match event? {
            PtyEvent::Output(bytes) => write_stdout(&bytes),
            PtyEvent::Ended(code) => return Ok(Some(code)),
            PtyEvent::Started(_) => {}
        }
    }
    Ok(None)
}

fn pump_input(target: &PtyTarget, pid: u32, session: &CancellationToken, runtime: &tokio::runtime::Handle) {
    let mut stdin = std::io::stdin();
    let mut buffer = [0u8; INPUT_CHUNK];
    while !session.is_cancelled() {
        let read = match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        if runtime.block_on(target.send_input(pid, &buffer[..read])).is_err() {
            return;
        }
    }
}

async fn pump_resize(target: PtyTarget, pid: u32, session: CancellationToken) {
    let Ok(mut winch) = signal(SignalKind::window_change()) else {
        return;
    };
    loop {
        tokio::select! {
            () = session.cancelled() => return,
            _ = winch.recv() => {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), target.resize(pid, terminal_size())).await;
            }
        }
    }
}

fn write_stdout(bytes: &[u8]) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

struct RawTerminal {
    saved: Termios,
}

impl RawTerminal {
    fn enter() -> Result<Self, String> {
        let saved = tcgetattr(std::io::stdin()).map_err(|e| format!("raw mode: {e}"))?;
        let mut raw = saved.clone();
        raw.make_raw();
        tcsetattr(std::io::stdin(), OptionalActions::Now, &raw).map_err(|e| format!("raw mode: {e}"))?;
        Ok(Self { saved })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = tcsetattr(std::io::stdin(), OptionalActions::Now, &self.saved);
    }
}
