//! A tiny shared helper: connect to a websocket URL, retrying with
//! exponential backoff on failure. Both `pumpfun` and `evm-onchain` are
//! long-running background listeners that need to survive a dropped
//! connection without the whole adapter (or the bot) going down, and
//! this is the one piece of that behaviour they'd otherwise each
//! duplicate.
//!
//! This crate deliberately does nothing else - no message parsing, no
//! protocol knowledge. That's each adapter's own concern.

use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::warn;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Connects to `url`, retrying with exponential backoff (capped at 30s)
/// on failure. Never gives up - a background listener task is expected
/// to run for the lifetime of the process, so "stop retrying" isn't a
/// valid outcome here, only "keep trying, slower."
pub async fn connect_with_backoff(url: &str) -> WsStream {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match connect_async(url).await {
            Ok((stream, _response)) => return stream,
            Err(e) => {
                warn!(url, error = %e, backoff_secs = backoff.as_secs(), "websocket connect failed, retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}
