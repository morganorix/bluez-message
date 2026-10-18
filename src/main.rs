use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use futures_util::StreamExt;

use bluez_message::addr::BtAddr;
use bluez_message::error::Result;
use bluez_message::serve::emit;
use bluez_message::{hfp, i18n, map::{Folder, MapClient}, t};

#[derive(Parser)]
#[command(about = "iPhone SMS/call relay over Bluetooth (MAP + HFP)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List a MAP folder (inbox, sent, outbox, deleted), one JSON per line
    List { addr: BtAddr, #[arg(default_value = "inbox")] folder: String },
    /// Send an SMS
    Send { addr: BtAddr, to: String, text: String },
    /// Watch the inbox by polling, one JSON per line
    Watch { addr: BtAddr, #[arg(long, default_value_t = 5)] interval: u64 },
    /// Raw RFCOMM hands-free unit (no audio), one JSON per line; requires `hfp_hf` disabled in WirePlumber
    Hfp { addr: BtAddr, #[arg(long)] answer: bool },
    /// MAP + HFP daemon driven by JSON lines (stdin/stdout), for the bar
    Serve { addr: BtAddr, #[arg(long, default_value_t = 5)] interval: u64 },
    /// Export the history (or one conversation) to a JSON file of the Downloads folder, via the daemon
    Export { #[arg(long)] key: Option<String> },
    /// Translation files (see locales/README.md)
    Locale {
        #[command(subcommand)]
        action: LocaleCmd,
    },
}

#[derive(Subcommand)]
enum LocaleCmd {
    /// Print the up-to-date template (all keys, empty values) to copy to ~/.config/bluez-message/locales/<lang>.json
    Template,
    /// Check a translation file: missing or unknown keys, {n} markers
    Check { file: std::path::PathBuf },
}

#[tokio::main]
async fn main() -> ExitCode {
    bluez_message::log::init();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{}", t!("error: {0}", e));
            ExitCode::FAILURE
        }
    }
}

/// Whole duration of a one-shot MAP command (connection included).
const ONESHOT_TIMEOUT: Duration = Duration::from_secs(120);

/// Bounds a one-shot command; on expiry the dropped session is released by obexd.
async fn oneshot<T>(f: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(ONESHOT_TIMEOUT, f)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "MAP"))?
}

/// Closes the session; the command error wins, a failed close is only logged.
async fn finish<T>(c: MapClient, r: Result<T>) -> Result<T> {
    if let Err(e) = c.close().await {
        tracing::warn!("MAP close: {e}");
    }
    r
}

/// `Ok(FAILURE)`: failure already reported (incomplete translation file).
async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::List { addr, folder } => {
            let folder: Folder = folder.parse()?;
            let list = oneshot(async {
                let c = MapClient::connect(&addr).await?;
                let r = c.list(folder).await;
                finish(c, r).await
            }).await?;
            for m in &list {
                if emit(m).is_err() {
                    break;
                }
            }
        }
        Cmd::Send { addr, to, text } => {
            oneshot(async {
                let c = MapClient::connect(&addr).await?;
                let r = c.send(&to, &text).await;
                finish(c, r).await
            }).await?;
        }
        Cmd::Watch { addr, interval } => {
            let c = MapClient::connect(&addr).await?;
            let r: Result<()> = async {
                let s = c.watch(Duration::from_secs(interval.max(1)));
                tokio::pin!(s);
                loop {
                    tokio::select! {
                        m = s.next() => match m { Some(m) => if emit(&m?).is_err() { break }, None => break },
                        _ = tokio::signal::ctrl_c() => break,
                    }
                }
                Ok(())
            }.await;
            finish(c, r).await?;
        }
        Cmd::Hfp { addr, answer } => {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            let run = hfp::run(&addr, rx, |ev| {
                match ev {
                    hfp::HfEvent::Clip { number } => tracing::info!(number = %hfp::mask(number), "CLIP"),
                    hfp::HfEvent::IncomingCall if answer => { let _ = tx.try_send(hfp::HfCmd::Answer); }
                    _ => {}
                }
                let _ = emit(ev);
            });
            tokio::select! {
                r = run => r?,
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Cmd::Serve { addr, interval } => {
            bluez_message::serve::serve(&addr, Duration::from_secs(interval.max(1))).await?;
        }
        Cmd::Export { key } => match tokio::task::spawn_blocking(move || export(key)).await.map_err(std::io::Error::other)?? {
            Ok(path) => println!("{path}"),
            Err(msg) => {
                eprintln!("{}", t!("error: {0}", msg));
                return Ok(ExitCode::FAILURE);
            }
        },
        Cmd::Locale { action: LocaleCmd::Template } => print!("{}", i18n::TEMPLATE),
        Cmd::Locale { action: LocaleCmd::Check { file } } => {
            let json = std::fs::read_to_string(&file)?;
            match i18n::check(&json) {
                Ok(issues) if issues.is_empty() => println!("{}", t!("{0}: complete", file.display())),
                Ok(issues) => {
                    for i in &issues {
                        println!("{i}");
                    }
                    eprintln!("{}", t!("{0} issue(s)", issues.len()));
                    return Ok(ExitCode::FAILURE);
                }
                Err(e) => {
                    eprintln!("{}", t!("invalid JSON: {0}", e));
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Asks the running daemon for an export ([`Request::Export`]) and waits for its answer, skipping the snapshot sent
/// first; returns the path of the written file, or the daemon's refusal (already translated).
fn export(key: Option<String>) -> Result<std::result::Result<String, String>> {
    use std::io::{BufRead, Read, Write};
    use bluez_message::engine::Cmd as Request;
    /// Snapshot lines can carry a whole conversation page: generous bound, still finite.
    const MAX: u64 = 16 * 1024 * 1024;
    let mut s = std::os::unix::net::UnixStream::connect(bluez_message::config::socket_path().map_err(std::io::Error::other)?)?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = serde_json::to_vec(&Request::Export { key }).map_err(std::io::Error::other)?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut r = std::io::BufReader::new(s);
    loop {
        line.clear();
        if (&mut r).take(MAX).read_until(b'\n', &mut line)? == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        if let Some(done) = serde_json::from_slice(&line).ok().and_then(export_reply) {
            return Ok(done);
        }
    }
}

/// Answer to [`bluez_message::engine::Cmd::Export`] among the daemon's lines; anything else (snapshot, broadcast errors) is skipped.
fn export_reply(out: bluez_message::engine::Out) -> Option<std::result::Result<String, String>> {
    use bluez_message::engine::Out;
    match out {
        Out::Exported { path } => Some(Ok(path)),
        Out::ExportFailed { msg } => Some(Err(msg)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::export_reply;
    use bluez_message::engine::Out;

    #[test]
    fn export_waits_for_its_own_answer() {
        assert_eq!(export_reply(Out::Error { msg: "calls unavailable".into() }), None, "broadcast error skipped");
        assert_eq!(export_reply(Out::ExportFailed { msg: "x".into() }), Some(Err("x".into())));
        assert_eq!(export_reply(Out::Exported { path: "/tmp/x.json".into() }), Some(Ok("/tmp/x.json".into())));
    }
}
