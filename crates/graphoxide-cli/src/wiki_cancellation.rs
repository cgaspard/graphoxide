//! Cooperative cancellation for owned Wiki CLI processes.
//!
//! The parent opts in explicitly and writes one nonce-authenticated line on
//! stdin. No source input is parsed here, and no signal handler changes other
//! CLI commands. Model workers may finish a request, but never publish files.

use crate::build_progress::{valid_run_nonce, BuildProgressMode, BUILD_PROGRESS_NONCE_ENV};
use anyhow::{ensure, Context as _};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

pub const WIKI_CANCEL_STDIN_ENV: &str = "GRAPHOXIDE_CANCEL_STDIN";
const CANCEL_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Default)]
pub struct WikiCancellation {
    cancelled: Arc<AtomicBool>,
    cooperative: bool,
}

impl WikiCancellation {
    pub fn from_stdin_environment(mode: BuildProgressMode) -> anyhow::Result<Self> {
        if std::env::var(WIKI_CANCEL_STDIN_ENV).ok().as_deref() != Some("1") {
            return Ok(Self::default());
        }
        ensure!(
            mode == BuildProgressMode::Json,
            "Wiki stdin cancellation requires --progress=json"
        );
        let nonce = std::env::var(BUILD_PROGRESS_NONCE_ENV)
            .context("Wiki stdin cancellation requires a progress nonce")?;
        ensure!(
            valid_run_nonce(&nonce),
            "Wiki stdin cancellation requires a valid progress nonce"
        );
        let cancellation = Self {
            cooperative: true,
            ..Self::default()
        };
        let flag = Arc::clone(&cancellation.cancelled);
        std::thread::Builder::new()
            .name("wiki-cancellation".into())
            .spawn(move || {
                if read_cancel_request(&mut std::io::stdin().lock(), &nonce) {
                    flag.store(true, Ordering::Release);
                }
            })
            .context("start Wiki cancellation listener")?;
        Ok(cancellation)
    }

    pub fn check(&self) -> anyhow::Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Acquire),
            "Wiki operation cancelled"
        );
        Ok(())
    }

    /// Run only the bounded provider request here; publication stays on the
    /// calling thread and therefore cannot race a cancellation rollback.
    pub(crate) fn model_request<T: Send + 'static>(
        &self,
        request: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        self.check()?;
        if !self.cooperative {
            return request();
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("wiki-model-request".into())
            .spawn(move || {
                let _ = sender.send(request());
            })
            .context("start Wiki model request")?;
        loop {
            self.check()?;
            match receiver.recv_timeout(CANCEL_POLL) {
                Ok(result) => {
                    self.check()?;
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("Wiki model request worker stopped")
                }
            }
        }
    }
}

fn read_cancel_request(input: &mut impl std::io::Read, nonce: &str) -> bool {
    let mut frame = [0_u8; 33];
    // An opted-in parent owns this pipe. EOF or a broken pipe means it can no
    // longer supervise publication, so unwind as if it explicitly cancelled.
    input.read_exact(&mut frame).is_err() || valid_cancel_frame(&frame, nonce)
}

fn valid_cancel_frame(frame: &[u8; 33], nonce: &str) -> bool {
    frame[32] == b'\n' && frame[..32] == *nonce.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_frame_is_fixed_and_authenticated() {
        let nonce = "0123456789abcdef0123456789abcdef";
        let mut frame = *b"0123456789abcdef0123456789abcdef\n";
        assert!(valid_cancel_frame(&frame, nonce));
        frame[0] = b'f';
        assert!(!valid_cancel_frame(&frame, nonce));
        frame[0] = b'0';
        frame[32] = b' ';
        assert!(!valid_cancel_frame(&frame, nonce));
    }

    #[test]
    fn closing_the_owned_stdin_channel_cancels_but_wrong_nonce_does_not() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert!(read_cancel_request(
            &mut std::io::Cursor::new(Vec::<u8>::new()),
            nonce
        ));
        assert!(read_cancel_request(
            &mut std::io::Cursor::new(b"0123"),
            nonce
        ));
        assert!(!read_cancel_request(
            &mut std::io::Cursor::new(b"ffffffffffffffffffffffffffffffff\n"),
            nonce
        ));
    }

    #[test]
    fn cancellation_returns_before_a_blocked_model_worker_finishes() {
        let cancellation = WikiCancellation {
            cooperative: true,
            ..Default::default()
        };
        let flag = Arc::clone(&cancellation.cancelled);
        let (started, receiver) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let canceller = std::thread::spawn(move || {
            receiver.recv_timeout(Duration::from_secs(2)).unwrap();
            flag.store(true, Ordering::Release);
        });
        let result = cancellation.model_request(move || {
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        });
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        release.send(()).unwrap();
        canceller.join().unwrap();
    }
}
