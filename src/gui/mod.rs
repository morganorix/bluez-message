//! Socle commun des fenêtres egui (`bluez-message`, `bluez-msg-call`) : thème, composants et démarrage
//! (runtime tokio, palette, client IPC du démon).

pub mod emoji;
pub mod theme;
pub mod widgets;

use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use tokio::sync::mpsc as tokio_mpsc;

use crate::engine::Cmd;
use crate::{config, ipc};

use theme::Palette;

/// Ce que reçoit la fenêtre : palette initiale, commandes vers le démon, événements du démon.
pub struct Client {
    pub palette: Palette,
    pub tx: tokio_mpsc::UnboundedSender<Cmd>,
    pub events: mpsc::Receiver<ipc::Link>,
}

/// Rendu Vulkan seul s'il existe un adaptateur : la valeur par défaut d'eframe (Vulkan + GL) charge aussi
/// le pilote GL de Mesa et LLVM, environ 110 Mo inutiles. Sans Vulkan, repli sur GL. `WGPU_BACKEND` garde
/// la main.
fn wgpu_setup(rt: &tokio::runtime::Runtime) -> eframe::egui_wgpu::WgpuSetup {
    use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew};
    use eframe::wgpu;
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    if wgpu::Backends::from_env().is_none() {
        let probe = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::PRIMARY, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let primary = !rt.block_on(probe.enumerate_adapters(wgpu::Backends::PRIMARY)).is_empty();
        tracing::debug!(primary, "wgpu backends");
        if primary {
            setup.instance_descriptor.backends = wgpu::Backends::PRIMARY;
        }
    }
    WgpuSetup::CreateNew(setup)
}

/// Ouvre la fenêtre `viewport` et construit l'application par `make`. Avec `connect`, le client IPC tourne
/// sur un runtime tokio et réveille l'affichage à chaque événement ; sans lui (démo), les commandes sont
/// sans effet. Les erreurs sont préfixées par `bin`.
pub fn run<A, F>(bin: &str, viewport: egui::ViewportBuilder, connect: bool, make: F) -> ExitCode
where
    A: eframe::App + 'static,
    F: FnOnce(Client) -> A + 'static,
{
    let path = match config::socket_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{bin}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Un seul thread : le client IPC n'a qu'une connexion à servir.
    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{bin}: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let handle = rt.handle().clone();
    let palette = theme::colors_path().map_or_else(Palette::default, |p| Palette::load(&p));
    let app_name = viewport.app_id.clone().unwrap_or_else(|| bin.to_owned());
    let mut opts = eframe::NativeOptions { viewport, ..Default::default() };
    opts.wgpu_options.wgpu_setup = wgpu_setup(&rt);
    let r = eframe::run_native(
        &app_name,
        opts,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            theme::apply(&ctx, &palette);
            let (ev_tx, ev_rx) = mpsc::channel();
            let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
            if connect {
                handle.spawn(ipc::client(path, cmd_rx, move |l| {
                    if ev_tx.send(l).is_ok() {
                        ctx.request_repaint();
                    }
                }));
            }
            Ok(Box::new(make(Client { palette, tx: cmd_tx, events: ev_rx })))
        }),
    );
    rt.shutdown_timeout(Duration::from_secs(1));
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{bin}: {e}");
            ExitCode::FAILURE
        }
    }
}
