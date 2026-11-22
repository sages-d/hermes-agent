//! Native embedded PTY + VT100 terminal emulator for Hermes (`portable-pty` + `vt100`).
//!
//! Spawns a real OS shell (login `$SHELL` on Unix) in a dedicated PTY, feeds output
//! into a `vt100::Parser` with scrollback, and pushes snapshots over a runtime-agnostic
//! `tokio::sync::mpsc` channel so GPUI wakes immediately with zero polling.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("failed to allocate PTY: {0}")]
    Pty(String),
    #[error("failed to spawn shell `{shell}`: {detail}")]
    Spawn { shell: String, detail: String },
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Snapshot of the visible terminal grid pushed to the UI on every update.
#[derive(Debug, Clone, Default)]
pub struct TerminalSnapshot {
    pub lines: Vec<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub rows: u16,
    pub cols: u16,
    pub exited: bool,
}

/// Handle to a live native PTY session. Remains alive while hidden in the UI.
pub struct NativeTerminal {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl NativeTerminal {
    /// Spawn the user's default interactive shell in `cwd`.
    pub fn spawn_shell(
        cwd: Option<PathBuf>,
        rows: u16,
        cols: u16,
    ) -> Result<(Self, mpsc::UnboundedReceiver<TerminalSnapshot>), TerminalError> {
        let shell = default_shell();
        let mut cmd = CommandBuilder::new(&shell);
        if cfg!(unix) {
            cmd.arg("-l");
        }
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        Self::spawn_command(cmd, &shell, rows, cols)
    }

    /// Spawn an arbitrary command in a PTY (used by tests and custom runners).
    pub fn spawn_command(
        cmd: CommandBuilder,
        label: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(Self, mpsc::UnboundedReceiver<TerminalSnapshot>), TerminalError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TerminalError::Pty(e.to_string()))?;

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TerminalError::Spawn {
                shell: label.to_string(),
                detail: e.to_string(),
            })?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TerminalError::Pty(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TerminalError::Pty(e.to_string()))?;

        let nz_rows = std::num::NonZeroU16::new(rows.max(4)).expect("non-zero");
        let nz_cols = std::num::NonZeroU16::new(cols.max(20)).expect("non-zero");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(nz_rows, nz_cols, 1000)));
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = mpsc::unbounded_channel();

        let reader_parser = Arc::clone(&parser);
        let reader_exited = Arc::clone(&exited);
        std::thread::Builder::new()
            .name("hermes-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let snap = {
                                let mut p = reader_parser.lock().expect("parser lock poisoned");
                                p.process(&buf[..n]);
                                snapshot_from_parser(&p, false)
                            };
                            if tx.send(snap).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = child.wait();
                reader_exited.store(true, std::sync::atomic::Ordering::SeqCst);
                let final_snap = {
                    let p = reader_parser.lock().expect("parser lock poisoned");
                    snapshot_from_parser(&p, true)
                };
                let _ = tx.send(final_snap);
            })
            .map_err(TerminalError::Io)?;

        Ok((
            Self {
                writer: Mutex::new(writer),
                master: Mutex::new(pair.master),
                parser,
                exited,
            },
            rx,
        ))
    }

    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        let mut w = self.writer.lock().expect("writer lock poisoned");
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), TerminalError> {
        let rows = rows.max(4);
        let cols = cols.max(20);
        {
            let m = self.master.lock().expect("master lock poisoned");
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TerminalError::Pty(e.to_string()))?;
        }
        let nz_rows = std::num::NonZeroU16::new(rows).expect("non-zero");
        let nz_cols = std::num::NonZeroU16::new(cols).expect("non-zero");
        let mut p = self.parser.lock().expect("parser lock poisoned");
        p.screen_mut().set_size(nz_rows, nz_cols);
        Ok(())
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        let p = self.parser.lock().expect("parser lock poisoned");
        let exited = self.exited.load(std::sync::atomic::Ordering::SeqCst);
        snapshot_from_parser(&p, exited)
    }
}

fn snapshot_from_parser(parser: &vt100::Parser, exited: bool) -> TerminalSnapshot {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let lines = screen.rows(0, cols.get()).collect();
    TerminalSnapshot {
        lines,
        cursor_row,
        cursor_col,
        rows: rows.get(),
        cols: cols.get(),
        exited,
    }
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawns_pty_command_and_captures_vt100_output() {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "printf 'hello-hermes-pty\\n'"]);

        let (term, mut rx) = NativeTerminal::spawn_command(cmd, "/bin/sh", 24, 80).unwrap();
        let mut seen = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Some(snap)) => {
                    if snap.lines.iter().any(|l| l.contains("hello-hermes-pty")) {
                        seen = true;
                    }
                    if snap.exited {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(seen, "expected PTY output in snapshot: {:?}", term.snapshot().lines);
    }
}
