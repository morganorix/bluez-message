//! Mode démon : le moteur piloté en JSON lignes.
//! Entrée (stdin) : `{"cmd":"send","to":…,"text":…}`, `{"cmd":"answer"}`, `{"cmd":"hangup"}`, `{"cmd":"quit"}`.
//! Sortie (stdout) : voir [`Out`]. Fin de stdin = arrêt.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::addr::BtAddr;
use crate::engine::{Cmd, Engine, Out, parse_cmd};
use crate::error::Result;

/// Écrit une valeur en JSON sur une ligne de stdout (format des sorties CLI et démon).
pub fn emit<T: serde::Serialize>(v: &T) {
    if let Ok(s) = serde_json::to_string(v) {
        println!("{s}");
    }
}

/// Boucle du mode `serve` : commandes JSON sur stdin, sorties sur stdout, jusqu'à `quit` ou la fin de stdin.
///
/// # Errors
/// Lecture de stdin impossible.
pub async fn serve(addr: &BtAddr, interval: Duration) -> Result<()> {
    let (engine, mut events) = Engine::start(addr, interval);
    let printer = tokio::spawn(async move {
        while let Some(o) = events.recv().await {
            emit(&o);
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
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
    // Les tâches du moteur sont terminées : le canal se ferme, l'impression se vide puis s'arrête.
    let _ = tokio::time::timeout(Duration::from_secs(1), printer).await;
    Ok(())
}
