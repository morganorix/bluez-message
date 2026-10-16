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
    /// Raw RFCOMM hands-free unit (no audio), one JSON per line; requires hfp_hf disabled in WirePlumber
    Hfp { addr: BtAddr, #[arg(long)] answer: bool },
    /// MAP + HFP daemon driven by JSON lines (stdin/stdout), for the bar
    Serve { addr: BtAddr, #[arg(long, default_value_t = 5)] interval: u64 },
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
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", t!("error: {0}", e));
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::List { addr, folder } => {
            let folder: Folder = folder.parse()?;
            let c = MapClient::connect(&addr).await?;
            let r = c.list(folder).await;
            c.close().await?;
            r?.iter().for_each(emit);
        }
        Cmd::Send { addr, to, text } => {
            let c = MapClient::connect(&addr).await?;
            let r = c.send(&to, &text).await;
            c.close().await?;
            r?;
        }
        Cmd::Watch { addr, interval } => {
            let c = MapClient::connect(&addr).await?;
            let r: Result<()> = async {
                let s = c.watch(Duration::from_secs(interval.max(1)));
                tokio::pin!(s);
                loop {
                    tokio::select! {
                        m = s.next() => match m { Some(m) => emit(&m?), None => break },
                        _ = tokio::signal::ctrl_c() => break,
                    }
                }
                Ok(())
            }.await;
            c.close().await?;
            r?;
        }
        Cmd::Hfp { addr, answer } => {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            let run = hfp::run(&addr, rx, |ev| {
                match ev {
                    hfp::HfEvent::Clip { number } => tracing::info!(number = %hfp::mask(number), "CLIP"),
                    hfp::HfEvent::IncomingCall if answer => { let _ = tx.try_send(hfp::HfCmd::Answer); }
                    _ => {}
                }
                emit(ev);
            });
            tokio::select! {
                r = run => r?,
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Cmd::Serve { addr, interval } => {
            bluez_message::serve::serve(&addr, Duration::from_secs(interval.max(1))).await?;
        }
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
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("{}", t!("invalid JSON: {0}", e));
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}
