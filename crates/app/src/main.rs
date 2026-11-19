#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_badge;
mod agent_hooks;
mod app_view;
mod assets;
mod cli;
mod codex_hooks;
mod colors;
mod daemon_client;
mod daemon_conn;
mod daemon_host;
mod daemon_restart;
mod diff_tabs;
mod diff_view;
mod element;
mod git_panel;
mod group_dialogs;
mod groups;
mod i18n;
mod input;
mod links;
mod notifications;
mod orch_cli;
mod orch_skill;
mod orch_watch;
mod os;
mod settings;
mod session;
mod settings_dialog;
mod sidebar;
mod single_instance;
mod tab_bar;
mod term_settings;
mod theme;
mod tray;
mod view;
mod win_caret;
mod win_flash;
mod win_show;
mod workspace;
mod worktree_dialogs;

use gpui_kit::{
    AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};

use crate::app_view::AppView;
use crate::session::{Rect, Session};
use crate::single_instance::Acquire;
use crate::tray::UiEvent;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `sich hook <agent>` runs on every agent hook event: dispatch before any other init. It
    // never returns (always exits 0, prints nothing).
    if args.first().is_some_and(|a| a == "hook") {
        sich_hooks::hook_main(args.get(1).map_or("", String::as_str));
    }
    // `sich orch <cmd>`: the orchestration CLI. Runs in place, never starts the GUI.
    if args.first().is_some_and(|a| a == "orch") {
        std::process::exit(orch_cli::run(&args[1..]));
    }
    // The GUI runs from a private copy so rebuilding the user-facing exe never hits a lock.
    if daemon_host::relocate_gui(&args) {
        return Ok(());
    }
    let socket = sich_daemon::socket_name_from_env();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => run_ui(socket, false),
        ["--takeover"] => run_ui(socket, true),
        ["daemon"] => cli::run_daemon(&socket),
        ["daemon", "start"] => cli::start_daemon(&socket),
        ["daemon", "stop"] => cli::stop_daemon(&socket),
        _ => {
            eprintln!("usage: sich [daemon [start|stop] | orch <cmd>]");
            std::process::exit(2);
        }
    }
}

fn run_ui(socket: String, force_takeover: bool) -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();
    let (events_tx, events_rx) = futures::channel::mpsc::unbounded::<UiEvent>();
    match rt.block_on(single_instance::acquire(&socket, sich_app::build_id(), force_takeover)) {
        Acquire::AlreadyRunning => return Ok(()),
        Acquire::Primary(listener) => {
            let tx = events_tx.clone();
            handle.spawn(single_instance::serve(listener, sich_app::build_id(), move |event| {
                let _ = tx.unbounded_send(event);
            }));
        }
        Acquire::Unavailable => {}
    }
    let session = Session::load();

    // Start the daemon if it is not running; the client keeps retrying if this fails.
    if let Err(e) = rt.block_on(cli::ensure_daemon(&socket)) {
        tracing::error!("could not start daemon: {e:#}");
    }

    gpui_kit::application().with_assets(crate::assets::AppAssets).run(move |cx| {
        gpui_kit::init(cx);
        term_settings::init(cx);
        settings::init(cx);
        theme::apply(cx);
        agent_hooks::startup(cx);
        codex_hooks::startup(cx);
        orch_skill::startup(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let displays: Vec<Rect> = cx
            .displays()
            .iter()
            .map(|d| {
                let b = d.bounds();
                Rect {
                    x: f32::from(b.origin.x),
                    y: f32::from(b.origin.y),
                    w: f32::from(b.size.width),
                    h: f32::from(b.size.height),
                }
            })
            .collect();
        let placement = session::window_placement(session.as_ref().and_then(|s| s.window.as_ref()), &displays);
        let win_size = size(px(placement.width), px(placement.height));
        let bounds = match placement.origin {
            Some((x, y)) => Bounds { origin: point(px(x), px(y)), size: win_size },
            None => Bounds::centered(None, win_size, cx),
        };
        let options = WindowOptions {
            window_bounds: Some(if placement.maximized {
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            }),
            titlebar: Some(TitlebarOptions {
                title: Some("Sich".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| {
                let mut view = AppView::new(handle, socket, (events_tx, events_rx), session, cx);
                view.watch_window(window, cx);
                view
            })
        })
        .expect("failed to open window");
        cx.activate(true);
    });

    drop(rt);
    Ok(())
}
