//! The Stratum V2 front-end: Noise-encrypted SV2 frames, a listener per pool, and one task per
//! connection holding its channels.

pub mod authority;
mod channel;
mod message;
mod session;
mod transport;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::net::TcpListener;

use self::authority::Authority;
use super::{Context, Transport};
use crate::mining::pool::Pool;

/// The transport every V2 session carries.
pub const TRANSPORT: Transport = Transport::V2Native;

/// Binds `bind` for `pool` and accepts connections in the background, each certified by `authority`. The
/// listener is bound when this returns.
pub async fn listen(
    bind: SocketAddr,
    pool: Arc<Pool>,
    context: Arc<Context>,
    authority: Arc<Authority>,
) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    tracing::info!(bind = %local, pool = %pool.id, transport = %TRANSPORT, "V2 listening");
    tokio::spawn(accept(listener, pool, context, authority));
    Ok(local)
}

async fn accept(
    listener: TcpListener,
    pool: Arc<Pool>,
    context: Arc<Context>,
    authority: Arc<Authority>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                // Acknowledgements are small frames; do not hold them back for coalescing.
                let _ = stream.set_nodelay(true);
                tokio::spawn(session::run(
                    stream,
                    peer,
                    pool.clone(),
                    context.clone(),
                    authority.clone(),
                ));
            }
            Err(e) => {
                // Out of file descriptors, most likely: back off rather than spin.
                tracing::warn!("V2 accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}
