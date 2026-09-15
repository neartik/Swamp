pub mod bridge;
pub mod jsonrpc;
pub mod server;
pub mod tools;

pub use bridge::run_bridge;
pub use jsonrpc::{Request, Response, RpcError};
pub use tools::{ToolSchema, wrap_untrusted};

use crate::dispatch::Dispatcher;
use crate::journal::JournalHandle;
use crate::journal::paths::RunPaths;
use camino::{Utf8Path, Utf8PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

const BRIDGE_SUBCOMMAND: &str = "mcp-bridge";
const SOCKET_FLAG: &str = "--socket";

pub struct McpServer {
    pub socket: Utf8PathBuf,
    pub disp: Arc<Dispatcher>,
    listener: UnixListener,
    view: Arc<JournalHandle>,
}

impl McpServer {
    /// Binds a UDS at paths.socket() with 0600 in a 0700 dir; removes it on drop.
    pub async fn bind(
        paths: &RunPaths,
        disp: Arc<Dispatcher>,
        view: Arc<JournalHandle>,
    ) -> anyhow::Result<(Self, Utf8PathBuf)> {
        let socket = paths.socket();
        let dir = socket.parent().unwrap_or(&paths.dir).to_path_buf();
        tokio::fs::create_dir_all(&dir).await?;
        set_mode(&dir, 0o700).await?;
        // A leftover socket from a crashed run would make bind fail with EADDRINUSE.
        if tokio::fs::symlink_metadata(&socket).await.is_ok() {
            tokio::fs::remove_file(&socket).await?;
        }
        let listener = UnixListener::bind(&socket)?;
        set_mode(&socket, 0o600).await?;
        Ok((
            McpServer {
                socket: socket.clone(),
                disp,
                listener,
                view,
            },
            socket,
        ))
    }

    pub fn serve(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.listener.accept().await {
                    Ok((stream, _)) => {
                        let disp = Arc::clone(&self.disp);
                        tokio::spawn(async move {
                            if let Err(e) = connection(stream, disp).await {
                                tracing::debug!("mcp connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("mcp listener stopped: {e}");
                        break;
                    }
                }
            }
        })
    }

    /// The journal the run is being written to, shared with the dispatcher.
    pub fn journal(&self) -> &Arc<JournalHandle> {
        &self.view
    }

    /// The JSON handed to `claude --mcp-config`: an absolute path, never the bare name.
    pub fn mcp_config_json(socket: &Utf8Path) -> String {
        let name = server::SERVER_NAME;
        serde_json::json!({
            "mcpServers": { name: { "command": exe(), "args": bridge_args(socket) } }
        })
        .to_string()
    }

    /// `codex -c mcp_servers.swamp.*`, ready to splice into an argv.
    pub fn codex_config_args(socket: &Utf8Path) -> Vec<String> {
        let name = server::SERVER_NAME;
        let args = serde_json::to_string(&bridge_args(socket)).unwrap_or_else(|_| "[]".to_owned());
        vec![
            "-c".to_owned(),
            format!("mcp_servers.{name}.command=\"{}\"", exe()),
            "-c".to_owned(),
            format!("mcp_servers.{name}.args={args}"),
        ]
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// `std::env::current_exe()`, never `swamp`: the brain spawns whatever binary is running.
pub fn exe() -> Utf8PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
        .unwrap_or_else(|| Utf8PathBuf::from(env!("CARGO_PKG_NAME")))
}

pub fn bridge_args(socket: &Utf8Path) -> Vec<String> {
    vec![
        BRIDGE_SUBCOMMAND.to_owned(),
        SOCKET_FLAG.to_owned(),
        socket.to_string(),
    ]
}

/// Requests are answered concurrently: a blocking `swamp_dispatch` must not stop
/// `swamp_status` from answering on the same connection.
async fn connection(stream: UnixStream, disp: Arc<Dispatcher>) -> anyhow::Result<()> {
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let disp = Arc::clone(&disp);
        let writer = Arc::clone(&writer);
        tokio::spawn(async move {
            let response = match jsonrpc::parse_line(&line) {
                Ok(req) => server::handle(&disp, req).await,
                Err(e) => Some(Response::err(None, e)),
            };
            let Some(response) = response else { return };
            let mut out = jsonrpc::encode(&response);
            out.push('\n');
            let mut w = writer.lock().await;
            if let Err(e) = w.write_all(out.as_bytes()).await {
                tracing::debug!("mcp write failed: {e}");
                return;
            }
            let _ = w.flush().await;
        });
    }
    Ok(())
}

async fn set_mode(path: &Utf8Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}
