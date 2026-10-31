//! The local end of a terminal: raw mode, resizes, frames.

use std::io::Write;
use std::os::fd::AsRawFd;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use zapper_policy::proto::Frame;

use zapper_core::util::read_frame;

pub fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

/// Rows and columns of the terminal on stdout.
pub fn size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

/// Puts stdin in raw mode until dropped.
struct Raw(libc::termios);

impl Raw {
    fn enter() -> Option<Raw> {
        let fd = std::io::stdin().as_raw_fd();
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
            return None;
        }
        let saved = t;
        unsafe {
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(fd, libc::TCSANOW, &t);
        }
        Some(Raw(saved))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(std::io::stdin().as_raw_fd(), libc::TCSANOW, &self.0) };
    }
}

/// Runs a terminal session over `io`; returns the remote exit code.
pub async fn run<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    io: T,
    tty: bool,
) -> Result<i32> {
    let _raw = if tty { Raw::enter() } else { None };
    let (mut r, mut w) = tokio::io::split(io);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);

    let stdin_tx = tx.clone();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 16384];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = stdin_tx.send(Frame::Eof).await;
                    break;
                }
                Ok(n) => {
                    if stdin_tx.send(Frame::Data(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    if tty {
        let winch_tx = tx.clone();
        tokio::spawn(async move {
            let Ok(mut winch) = signal(SignalKind::window_change()) else {
                return;
            };
            while winch.recv().await.is_some() {
                if let Some((rows, cols)) = size() {
                    if winch_tx.send(Frame::Resize(rows, cols)).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
    drop(tx);
    tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            if w.write_all(&f.encode()).await.is_err() {
                break;
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    loop {
        match read_frame(&mut r).await? {
            Some(Frame::Data(d)) => {
                stdout.write_all(&d)?;
                stdout.flush()?;
            }
            Some(Frame::Stderr(d)) => {
                stderr.write_all(&d)?;
                stderr.flush()?;
            }
            Some(Frame::Exit(code)) => return Ok(code),
            Some(_) => {}
            None => return Ok(255),
        }
    }
}
