//! Daemon mode: the engine driven through JSON lines.
//! Input (stdin): `{"cmd":"send","to":…,"text":…}`, `{"cmd":"answer"}`, `{"cmd":"hangup"}`, `{"cmd":"quit"}`.
//! Output (stdout): see [`Out`]. End of stdin = stop.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::addr::BtAddr;
use crate::engine::{Cmd, Engine, Out, parse_cmd};
use crate::error::Result;
use crate::ipc::MAX_LINE;
use crate::t;

/// Writes a value as JSON on one stdout line (format of CLI and daemon outputs).
pub fn emit<T: serde::Serialize>(v: &T) {
    if let Ok(s) = serde_json::to_string(v) {
        println!("{s}");
    }
}

/// `serve` mode loop: JSON commands on stdin, outputs on stdout, until `quit` or end of stdin.
///
/// # Errors
/// Reading stdin impossible. A line longer than [`MAX_LINE`] bytes ends the loop, like on the socket.
pub async fn serve(addr: &BtAddr, interval: Duration) -> Result<()> {
    let (engine, mut events) = Engine::start(addr, interval);
    let printer = tokio::spawn(async move {
        while let Some(o) = events.recv().await {
            emit(&o);
        }
    });

    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if (&mut stdin).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await? == 0 {
            break;
        }
        if buf.last() != Some(&b'\n') && buf.len() > MAX_LINE {
            emit(&Out::Error { msg: t!("line longer than {0} bytes rejected", MAX_LINE) });
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        let r = match parse_cmd(&line) {
            Ok(Cmd::Quit) => break,
            Ok(cmd) => engine.submit(cmd),
            Err(e) => Err(e),
        };
        if let Err(msg) = r {
            emit(&Out::Error { msg });
        }
    }
    engine.shutdown().await;
    // The engine tasks are finished: the channel closes, printing drains then stops.
    let _ = tokio::time::timeout(Duration::from_secs(1), printer).await;
    Ok(())
}
