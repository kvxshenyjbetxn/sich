#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_view;
mod assets;
mod cli;
mod colors;
mod daemon_client;
mod daemon_conn;
mod daemon_kill;
mod element;
mod i18n;
mod input;
mod links;
mod notifications;
mod os;
mod settings;
mod settings_dialog;
mod sidebar;
mod tab_bar;
mod term_settings;
mod theme;
mod view;
mod win_caret;
mod workspace;
mod worktree_dialogs;

use gpui_kit::{AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

use crate::app_view::AppView;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `sich hook <agent>` runs on every agent hook event: dispatch before any other init. It
    // never returns (always exits 0, prints nothing).
    if args.first().is_some_and(|a| a == "hook") {
        sich_hooks::hook_main(args.get(1).map_or("", String::as_str));
    }
    let socket = sich_daemon::socket_name_from_env();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => run_ui(socket),
        ["daemon"] => cli::run_daemon(&socket),
        ["daemon", "stop"] => cli::stop_daemon(&socket),
        _ => {
            eprintln!("usage: sich [daemon [stop]]");
            std::process::exit(2);
        }
    }
}

fn run_ui(socket: String) -> anyhow::Result<()> {
    if cfg!(debug_assertions) {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();

    // Start the daemon if it is not running; the client keeps retrying if this fails.
    if let Err(e) = rt.block_on(cli::ensure_daemon(&socket)) {
        tracing::error!("could not start daemon: {e:#}");
    }

    gpui_kit::application().with_assets(crate::assets::AppAssets).run(move |cx| {
        gpui_kit::init(cx);
        term_settings::init(cx);
        settings::init(cx);
        theme::apply(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Sich".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |_window, cx| {
            cx.new(|cx| AppView::new(handle, socket, cx))
        })
        .expect("failed to open window");
        cx.activate(true);
    });

    drop(rt);
    Ok(())
}
