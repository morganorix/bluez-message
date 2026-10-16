//! Common base of the egui windows (`bluez-message`, `bluez-msg-call`): theme, components and startup
//! (tokio runtime, palette, daemon IPC client).

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

/// What the window receives: initial palette, commands to the daemon, daemon events.
pub struct Client {
    pub palette: Palette,
    pub tx: tokio_mpsc::UnboundedSender<Cmd>,
    pub events: mpsc::Receiver<ipc::Link>,
}

/// Vulkan-only rendering if there is an adapter: eframe's default (Vulkan + GL) also loads
/// Mesa's GL driver and LLVM, about 110 MB for nothing. Without Vulkan, falls back to GL. `WGPU_BACKEND` keeps
/// priority.
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

/// Opens the `viewport` window and builds the app with `make`. With `connect`, the IPC client runs
/// on a tokio runtime and wakes the display on each event; without it (demo), commands have
/// no effect. Errors are prefixed with `bin`.
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
    // Single thread: the IPC client has only one connection to serve.
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
