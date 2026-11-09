//! The Stratum V1 front-end: a listener, and one task per connection.

mod message;
mod session;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::net::TcpListener;

use super::{Context, Transport};
use crate::mining::pool::Pool;

/// The transport every V1 session carries.
pub const TRANSPORT: Transport = Transport::V1;

/// Binds `bind` for `pool` and accepts connections in the background. The listener is bound when this
/// returns.
pub async fn listen(
    bind: SocketAddr,
    pool: Arc<Pool>,
    context: Arc<Context>,
) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    tracing::info!(bind = %local, pool = %pool.id, transport = %TRANSPORT, "V1 listening");
    tokio::spawn(accept(listener, pool, context));
    Ok(local)
}

async fn accept(listener: TcpListener, pool: Arc<Pool>, context: Arc<Context>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                // Replies are single small lines; do not hold them back for coalescing.
                let _ = stream.set_nodelay(true);
                tokio::spawn(session::run(stream, peer, pool.clone(), context.clone()));
            }
            Err(e) => {
                // Out of file descriptors, most likely: back off rather than spin.
                tracing::warn!("V1 accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}
