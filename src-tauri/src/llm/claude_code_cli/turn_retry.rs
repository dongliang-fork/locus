//! Retry the native *conversation*, never a tool or an unverified input payload.
//! Only transport boundaries tag recoverable errors; provider/tool/auth/history
//! errors must not become retryable through a broad keyword match.
use std::{future::Future, time::Duration};

const DISCONNECTED: &str = "Claude transport disconnected: ";
pub(crate) const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

pub(super) fn disconnected(reason: impl AsRef<str>) -> String {
    format!("{DISCONNECTED}{}", reason.as_ref())
}

pub(crate) fn is_disconnected(error: &str) -> bool {
    error.starts_with(DISCONNECTED)
}

// OpenSSH has no structured startup verdict. Classify its bounded diagnostic
// only at the SSH startup boundary, never arbitrary model/tool error text.
pub(super) fn startup_rejected(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "permission denied",
        "host key verification failed",
        "remote host identification has changed",
        "invalid",
        "not found",
        "keychain",
        "authentication",
        "not logged in",
        "plugin validation failed",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

pub(super) fn ssh_startup_error(error: String) -> String {
    if startup_rejected(&error) {
        return error
            .strip_prefix(DISCONNECTED)
            .unwrap_or(&error)
            .to_string();
    }
    if is_disconnected(&error) {
        return error;
    }
    let lower = error.to_ascii_lowercase();
    if [
        "connection timed out",
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "network is unreachable",
        "no route to host",
        "operation timed out",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        disconnected(error)
    } else {
        error
    }
}

/// The caller creates a fresh host and synchronizes native history on every
/// attempt after the first. `before_retry` retires the old process and preserves
/// partial output. Cancellation covers startup, synchronization and backoff too.
pub(crate) async fn run<T, A, AF, R, RF>(
    mut attempt: A,
    mut before_retry: R,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    delays: &[Duration],
) -> Result<Option<T>, String>
where
    A: FnMut(u32) -> AF,
    AF: Future<Output = Result<T, String>>,
    R: FnMut(u32, String) -> RF,
    RF: Future<Output = Result<(), String>>,
{
    for index in 0..=delays.len() {
        if *cancel.borrow() {
            return Ok(None);
        }
        let result = tokio::select! {
            biased;
            _ = cancel.changed() => return Ok(None),
            result = attempt(index as u32 + 1) => result,
        };
        let error = match result {
            Ok(value) => return Ok(Some(value)),
            Err(error) if is_disconnected(&error) => error,
            Err(error) => return Err(error),
        };
        let Some(delay) = delays.get(index) else {
            return Err(format!("Claude connection recovery failed after {} retries. Resume the conversation when the connection is available. Last error: {error}", delays.len()));
        };
        tokio::select! {
            biased;
            _ = cancel.changed() => return Ok(None),
            result = before_retry(index as u32 + 1, error) => result?,
        }
        tokio::select! {
            biased;
            _ = cancel.changed() => return Ok(None),
            _ = tokio::time::sleep(*delay) => {},
        }
    }
    unreachable!("bounded retry loop returns on its last attempt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    #[test]
    fn only_transport_boundaries_opt_into_recovery() {
        for diagnostic in [
            "ssh: Connection timed out",
            "ssh: connect: Connection refused",
            "Connection reset by peer",
        ] {
            assert!(is_disconnected(&ssh_startup_error(diagnostic.into())));
        }
        for diagnostic in [
            "Permission denied (publickey)",
            "Host key verification failed",
            "Invalid remote history",
            "Claude plugin not found",
            "keychain authentication timed out",
        ] {
            assert!(!is_disconnected(&ssh_startup_error(diagnostic.into())));
        }
        for error in [
            "Claude bridge closed",
            "tool connection timed out",
            "API error (429)",
            "steering outcome unknown",
        ] {
            assert!(!is_disconnected(error));
        }
    }

    #[tokio::test]
    async fn disconnect_resynchronizes_before_each_fresh_attempt() {
        let (_send, receive) = tokio::sync::watch::channel(false);
        let synchronized = Arc::new(AtomicU32::new(0));
        let result = run(
            |attempt| {
                assert_eq!(synchronized.load(Ordering::SeqCst), attempt - 1);
                async move {
                    if attempt < 3 {
                        Err(disconnected("bridge closed"))
                    } else {
                        Ok("continued")
                    }
                }
            },
            |attempt, _| {
                synchronized.store(attempt, Ordering::SeqCst);
                async { Ok(()) }
            },
            receive,
            &[Duration::ZERO; 3],
        )
        .await
        .unwrap();
        assert_eq!(result, Some("continued"));
    }

    #[tokio::test]
    async fn recovery_is_bounded_and_does_not_retry_validation_errors() {
        let (_send, receive) = tokio::sync::watch::channel(false);
        let count = AtomicU32::new(0);
        let error = run::<(), _, _, _, _>(
            |_| {
                count.fetch_add(1, Ordering::SeqCst);
                async { Err(disconnected("offline")) }
            },
            |_, _| async { Ok(()) },
            receive.clone(),
            &[Duration::ZERO; 3],
        )
        .await
        .unwrap_err();
        assert_eq!(count.load(Ordering::SeqCst), 4);
        assert!(error.contains("after 3 retries"));
        let error = run::<(), _, _, _, _>(
            |_| async { Err("Unverified native history".into()) },
            |_, _| async { panic!("must not retry validation failures") },
            receive,
            &[Duration::ZERO; 3],
        )
        .await
        .unwrap_err();
        assert_eq!(error, "Unverified native history");
    }

    #[tokio::test]
    async fn cancel_interrupts_attempt_and_backoff_without_another_submission() {
        let (send, receive) = tokio::sync::watch::channel(false);
        let result = run::<(), _, _, _, _>(
            |_| {
                send.send(true).unwrap();
                std::future::pending()
            },
            |_, _| async { panic!("cancel is not a disconnect") },
            receive,
            &RETRY_DELAYS,
        )
        .await
        .unwrap();
        assert_eq!(result, None);
        let (send, receive) = tokio::sync::watch::channel(false);
        let count = AtomicU32::new(0);
        let result = run::<(), _, _, _, _>(
            |_| {
                count.fetch_add(1, Ordering::SeqCst);
                async { Err(disconnected("offline")) }
            },
            |_, _| {
                send.send(true).unwrap();
                async { Ok(()) }
            },
            receive,
            &RETRY_DELAYS,
        )
        .await
        .unwrap();
        assert_eq!(result, None);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
