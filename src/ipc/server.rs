// Author: Dustin Pilgrim
// License: GPL-3.0-only

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
    sync::{mpsc, watch},
};

use crate::core::{info::WatchEvent, manager_msg::ManagerMsg};

pub async fn spawn_ipc_server(
    tx: mpsc::Sender<ManagerMsg>,
    watch_rx: watch::Receiver<WatchEvent>,
    verbose: bool,
) -> Result<(), String> {
    let path = crate::ipc::socket_path()?;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Remove stale socket file (if any). Ignore errors.
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)
        .map_err(|e| format!("failed to bind ipc socket {}: {e}", path.display()))?;

    eventline::info!("ipc: listening on {}", path.display());

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    eventline::error!("ipc: accept failed: {}", e);
                    continue;
                }
            };

            let tx = tx.clone();
            let watch_rx = watch_rx.clone();
            tokio::spawn(async move {
                // Read the whole request (client must shutdown its write-half)
                let mut buf = Vec::new();
                if let Err(e) = stream.read_to_end(&mut buf).await {
                    eventline::warn!("ipc: read failed: {}", e);
                    return;
                }

                let cmd = String::from_utf8_lossy(&buf).trim().to_string();
                if cmd.is_empty() {
                    let _ = stream.write_all(b"ERROR: empty command").await;
                    let _ = stream.shutdown().await; // close response
                    return;
                }

                if cmd == "watch" {
                    if let Err(e) = stream_watch(stream, watch_rx).await {
                        eventline::debug!("ipc: watch stream ended: {e}");
                    }
                    return;
                }

                let response = if verbose {
                    eventline::scope!("ipc", {
                        eventline::debug!("ipc: command: {}", cmd);
                        crate::ipc::router::route_command(&cmd, &tx).await
                    })
                } else {
                    crate::ipc::router::route_command(&cmd, &tx).await
                };

                if let Err(e) = stream.write_all(response.as_bytes()).await {
                    eventline::warn!("ipc: write failed: {}", e);
                    return;
                }

                let _ = stream.shutdown().await;
            });
        }
    });

    Ok(())
}

async fn stream_watch(
    mut stream: tokio::net::UnixStream,
    mut watch_rx: watch::Receiver<WatchEvent>,
) -> std::io::Result<()> {
    loop {
        let event = watch_rx.borrow_and_update().clone();
        let Ok(line) = serde_json::to_string(&event) else {
            eventline::warn!("ipc: could not encode watch event");
            return Ok(());
        };

        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;

        if watch_rx.changed().await.is_err() {
            return Ok(());
        }
    }
}
