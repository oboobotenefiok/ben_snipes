//! A small retry-with-backoff helper for the transient-failure-prone
//! network calls throughout this crate (RPC calls, PumpPortal/DexScreener/
//! RugCheck/Jupiter HTTP requests).
//!
//! **Every call site this is used on has been checked for idempotency
//! before wrapping it** - retrying isn't free to reach for blindly.
//! Reads (balance checks, price/metrics/safety lookups) are always safe
//! to retry. The two calls that "do" something - requesting an unsigned
//! transaction from PumpPortal, and broadcasting a signed one - are also
//! safe here specifically: `trade-local` is a stateless "build me a
//! transaction" request with no server-side side effect, and
//! resubmitting the exact same signed transaction bytes to
//! `sendTransaction` is a standard, safe pattern in Solana tooling (the
//! network treats a duplicate submission as a no-op, not a double
//! execution). Don't reach for this on a call that isn't verified safe
//! to repeat.

use std::future::Future;
use std::time::Duration;
use tracing::debug;

const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Calls `f` up to `max_attempts` times, with exponential backoff
/// between failures, returning the first success or the last error if
/// every attempt fails.
pub async fn with_retry<F, Fut, T>(max_attempts: u32, mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut backoff = INITIAL_BACKOFF;
    let mut last_error = String::from("max_attempts was 0");

    for attempt in 1..=max_attempts.max(1) {
        match f().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                debug!(attempt, max_attempts, error = %e, "attempt failed, will retry" );
                last_error = e;
                if attempt < max_attempts {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    Err(format!("failed after {max_attempts} attempts, last error: {last_error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn succeeds_immediately_without_retrying() {
        let calls = AtomicU32::new(0);
        let result = with_retry(3, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(42)
        })
        .await;

        assert_eq!(result, Ok(42));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_until_success() {
        let calls = AtomicU32::new(0);
        let result = with_retry(5, || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err("transient".to_string())
            } else {
                Ok(99)
            }
        })
        .await;

        assert_eq!(result, Ok(99));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = AtomicU32::new(0);
        let result: Result<u32, String> = with_retry(3, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("always fails".to_string())
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
