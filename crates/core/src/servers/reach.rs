//! Reachability for the Servers view: a TCP connect to the SSH, RDP or VNC port. No
//! login, no bytes sent; at most [`super::REACH_CONCURRENCY`] attempts at once, each
//! with a timeout, on the core runtime, until cancelled.

use std::time::{Duration, Instant};

use futures::StreamExt as _;
use switchyard_store::ProfileId;
use tokio_util::sync::CancellationToken;

/// One profile's reachability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReachStatus {
    /// The port accepted a connection after this many milliseconds.
    Up(u32),
    /// Refused, timed out or not resolvable.
    Down(String),
    /// Not checked from here (reached through a jump Host or a tunnel).
    Indirect,
}

/// Something to check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachTarget {
    /// Profile.
    pub id: ProfileId,
    /// Address (`None`: indirect, not checked).
    pub address: Option<String>,
    /// Port.
    pub port: u16,
}

/// Connect once to `address:port`.
pub async fn check(address: &str, port: u16, timeout: Duration) -> ReachStatus {
    let start = Instant::now();
    let addr = if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    };
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => ReachStatus::Up(start.elapsed().as_millis().min(u128::from(u32::MAX)) as u32),
        Ok(Err(e)) => ReachStatus::Down(e.kind().to_string()),
        Err(_) => ReachStatus::Down("timed out".into()),
    }
}

/// Check every target once, `concurrency` at a time, reporting each as it finishes.
/// Stops early when `cancel` fires.
pub async fn round(
    targets: &[ReachTarget],
    concurrency: usize,
    timeout: Duration,
    cancel: &CancellationToken,
    report: &(dyn Fn(ProfileId, ReachStatus) + Send + Sync),
) {
    let mut checks = futures::stream::iter(targets.iter().cloned())
        .map(|t| async move {
            let status = match &t.address {
                Some(a) => check(a, t.port, timeout).await,
                None => ReachStatus::Indirect,
            };
            (t.id, status)
        })
        .buffer_unordered(concurrency.max(1));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            next = checks.next() => match next {
                Some((id, status)) => report(id, status),
                None => return,
            },
        }
    }
}

/// Rounds every `interval` until `cancel` fires.
pub async fn run(
    targets: Vec<ReachTarget>,
    interval: Duration,
    concurrency: usize,
    timeout: Duration,
    cancel: CancellationToken,
    report: impl Fn(ProfileId, ReachStatus) + Send + Sync,
) {
    loop {
        round(&targets, concurrency, timeout, &cancel, &report).await;
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn open_and_closed_ports() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap().port();
        // A port nobody listens on: bind, note it, close.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let targets = vec![
            ReachTarget {
                id: ProfileId("up".into()),
                address: Some("127.0.0.1".into()),
                port: open,
            },
            ReachTarget {
                id: ProfileId("down".into()),
                address: Some("127.0.0.1".into()),
                port: closed,
            },
            ReachTarget {
                id: ProfileId("via".into()),
                address: None,
                port: 22,
            },
        ];
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        round(
            &targets,
            2,
            Duration::from_secs(2),
            &CancellationToken::new(),
            &move |id, st| s.lock().unwrap().push((id.0, st)),
        )
        .await;
        let mut seen = seen.lock().unwrap().clone();
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(seen.len(), 3);
        assert!(matches!(seen[0], (ref id, ReachStatus::Down(_)) if id == "down"));
        assert!(matches!(seen[1], (ref id, ReachStatus::Up(_)) if id == "up"));
        assert_eq!(seen[2], ("via".into(), ReachStatus::Indirect));
        drop(listener);
    }

    #[tokio::test]
    async fn runs_on_a_timer_until_cancelled() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = Arc::new(Mutex::new(0usize));
        let c = count.clone();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            vec![ReachTarget {
                id: ProfileId("a".into()),
                address: Some("127.0.0.1".into()),
                port,
            }],
            Duration::from_millis(30),
            4,
            Duration::from_secs(1),
            cancel.clone(),
            move |_, _| *c.lock().unwrap() += 1,
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(*count.lock().unwrap() >= 2, "several rounds ran");
    }
}
