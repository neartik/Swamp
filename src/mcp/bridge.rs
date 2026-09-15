use anyhow::Context;
use camino::Utf8Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

const CHUNK: usize = 8 * 1024;

/// stdio <-> UDS byte pump, zero logic. The bridge exists because both CLIs spawn MCP
/// servers as stdio children, and the real server lives in the Swamp process that owns
/// the dispatcher.
pub async fn run_bridge(socket: &Utf8Path) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket.as_std_path())
        .await
        .with_context(|| format!("connecting to the swamp control socket at {socket}"))?;
    let (down, up) = stream.into_split();

    let mut upward = tokio::spawn(async move { pump(tokio::io::stdin(), up).await });
    let mut downward = tokio::spawn(async move { pump(down, tokio::io::stdout()).await });
    // Either end hanging up ends the bridge: the socket closing means the run is over, and
    // stdin closing is how a CLI shuts its MCP server down.
    let done = tokio::select! {
        r = &mut upward => { downward.abort(); r }
        r = &mut downward => { upward.abort(); r }
    };
    match done {
        Ok(r) => r,
        Err(e) if e.is_cancelled() => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn pump<R, W>(mut from: R, mut to: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).await?;
        // NDJSON is interactive: a buffered response is a hung tool call.
        to.flush().await?;
    }
    let _ = to.shutdown().await;
    Ok(())
}
