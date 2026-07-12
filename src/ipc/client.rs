// Author: Dustin Pilgrim
// License: GPL-3.0-only

use std::io::Write as _;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{Duration, timeout},
};

/// Print the current state and each later state transition as newline-delimited
/// JSON. The daemon closes the stream when it stops.
pub async fn watch() -> Result<(), String> {
    let path = crate::ipc::socket_path()?;
    let mut stream = timeout(Duration::from_secs(2), UnixStream::connect(&path))
        .await
        .map_err(|_| "timeout connecting to daemon".to_string())?
        .map_err(|_| {
            format!(
                "failed to connect to {}: daemon not running",
                path.display()
            )
        })?;

    timeout(Duration::from_secs(2), stream.write_all(b"watch"))
        .await
        .map_err(|_| "timeout writing to daemon".to_string())?
        .map_err(|e| format!("write failed: {e}"))?;
    stream
        .shutdown()
        .await
        .map_err(|e| format!("shutdown failed: {e}"))?;

    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("read failed: {e}"))?
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{line}").map_err(|e| format!("stdout write failed: {e}"))?;
        stdout
            .flush()
            .map_err(|e| format!("stdout flush failed: {e}"))?;
    }

    Ok(())
}

pub async fn send_raw(cmd: &str) -> Result<String, String> {
    let path = crate::ipc::socket_path()?;

    // Local fallback for `dump` (does not require daemon).
    async fn dump_fallback(cmd: &str) -> Option<String> {
        let trimmed = cmd.trim_start();
        let rest = trimmed.strip_prefix("dump")?;
        Some(crate::ipc::handlers::dump::handle_dump(rest).await)
    }

    // If socket file doesn't exist, allow dump to run offline.
    if !path.exists() {
        if let Some(out) = dump_fallback(cmd).await {
            return Ok(out);
        }
        return Err("daemon not running".to_string());
    }

    let mut stream = match timeout(Duration::from_secs(2), UnixStream::connect(&path)).await {
        Ok(Ok(s)) => s,
        Ok(Err(_e)) => {
            // Socket exists but nothing is listening (stale socket / crashed daemon).
            if let Some(out) = dump_fallback(cmd).await {
                return Ok(out);
            }
            return Err(format!(
                "failed to connect to {}: daemon not running",
                path.display()
            ));
        }
        Err(_) => {
            if let Some(out) = dump_fallback(cmd).await {
                return Ok(out);
            }
            return Err("timeout connecting to daemon".to_string());
        }
    };

    timeout(Duration::from_secs(2), stream.write_all(cmd.as_bytes()))
        .await
        .map_err(|_| "timeout writing to daemon".to_string())?
        .map_err(|e| format!("write failed: {e}"))?;

    timeout(Duration::from_secs(2), stream.shutdown())
        .await
        .map_err(|_| "timeout finalizing request".to_string())?
        .map_err(|e| format!("shutdown failed: {e}"))?;

    let mut resp = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut resp))
        .await
        .map_err(|_| "timeout reading response".to_string())?
        .map_err(|e| format!("read failed: {e}"))?;

    Ok(String::from_utf8_lossy(&resp).to_string())
}
