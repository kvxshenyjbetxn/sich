//! "Phone" section of the settings dialog: runs the phone gateway on demand, shows its status and
//! paired devices, and drives the pairing dialog (QR code + confirmation).
//!
//! All gateway IPC runs on the GUI's tokio runtime; results come back through one channel that a
//! GPUI task drains into the `PhoneState` global. Everything that can be decided without GPUI is a
//! pure function (state machine, QR layout, text formatting) and unit-tested.

use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AsyncApp, ClipboardItem, Global, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div, px, rgb,
};
use sich_gateway::{ControlClient, Device, PairRequest, PairingInfo};
use tokio::runtime::Handle;

use crate::i18n::{Language, Tr, tr};
use crate::settings;

/// Quiet zone around the QR code, in modules (the QR spec asks for 4).
const QR_QUIET: usize = 4;
/// Target side of the QR panel in px.
const QR_TARGET_PX: usize = 240;
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const ID_CHARS: usize = 10;

// ----- pure logic -----

/// What the section shows about the gateway.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum GatewayStatus {
    /// Nothing known yet (first poll in flight).
    #[default]
    Unknown,
    Starting,
    NotRunning,
    Running { endpoint_id: String, connected: u32 },
    Error(String),
}

pub fn short_id(id: &str) -> String {
    id.chars().take(ID_CHARS).collect()
}

/// Status line text and whether it is an error.
pub fn status_line(lang: Language, status: &GatewayStatus) -> (String, bool) {
    match status {
        GatewayStatus::Unknown => ("…".to_string(), false),
        GatewayStatus::Starting => (tr(lang, Tr::PhoneStarting).to_string(), false),
        GatewayStatus::NotRunning => (tr(lang, Tr::PhoneNotRunning).to_string(), false),
        GatewayStatus::Error(message) => (message.clone(), true),
        GatewayStatus::Running { endpoint_id, connected } => {
            let id = short_id(endpoint_id);
            let text = match lang {
                Language::Uk => format!("Працює · ID {id} · Підключено: {connected}"),
                Language::Ru => format!("Работает · ID {id} · Подключено: {connected}"),
                Language::En => format!("Running · ID {id} · Connected: {connected}"),
            };
            (text, false)
        }
    }
}

/// What the settings row knows about the firewall rule (Windows only).
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FirewallUi {
    /// Not checked yet.
    #[default]
    Unknown,
    Allowed,
    Blocked,
    /// A check or the UAC flow is running.
    Busy,
}

/// Outcome note shown next to the firewall line.
#[cfg(windows)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FirewallNote {
    #[default]
    None,
    Cancelled,
    Error(String),
}

/// Text of the firewall row and whether it is an error.
#[cfg(windows)]
pub fn firewall_line(lang: Language, state: FirewallUi, note: &FirewallNote) -> (String, bool) {
    let head = tr(lang, Tr::PhoneFwDirect);
    let value = match state {
        FirewallUi::Allowed => tr(lang, Tr::PhoneFwAllowed),
        FirewallUi::Blocked => tr(lang, Tr::PhoneFwBlocked),
        FirewallUi::Unknown | FirewallUi::Busy => "…",
    };
    match note {
        FirewallNote::None => (format!("{head}: {value}"), false),
        FirewallNote::Cancelled => (format!("{head}: {value} · {}", tr(lang, Tr::PhoneFwCancelled)), false),
        FirewallNote::Error(e) => (format!("{head}: {value} · {e}"), true),
    }
}

/// Maps the result of an elevated action plus the re-check to what the row shows.
#[cfg(windows)]
pub fn firewall_after(outcome: crate::phone_firewall::Outcome, present: bool) -> (FirewallUi, FirewallNote) {
    use crate::phone_firewall::Outcome;
    let state = if present { FirewallUi::Allowed } else { FirewallUi::Blocked };
    let note = match outcome {
        Outcome::Done => FirewallNote::None,
        Outcome::Cancelled => FirewallNote::Cancelled,
        Outcome::Failed(e) => FirewallNote::Error(e),
    };
    (state, note)
}

/// `m:ss` of the time left; zero once expired.
pub fn format_countdown(remaining_ms: u64) -> String {
    let secs = remaining_ms.div_ceil(1000);
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub fn valid_for_text(lang: Language, remaining_ms: u64) -> String {
    let left = format_countdown(remaining_ms);
    match lang {
        Language::Uk => format!("Дійсний ще {left}"),
        Language::Ru => format!("Действителен ещё {left}"),
        Language::En => format!("Valid for {left}"),
    }
}

pub fn allow_text(lang: Language, name: &str, endpoint_id: &str) -> String {
    let id = short_id(endpoint_id);
    match lang {
        Language::Uk => format!("Дозволити «{name}» ({id})?"),
        Language::Ru => format!("Разрешить «{name}» ({id})?"),
        Language::En => format!("Allow \"{name}\" ({id})?"),
    }
}

/// "5 min ago" style text for `then_ms` seen from `now_ms`.
pub fn relative_time(lang: Language, now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    let (n, unit) = if secs < 60 {
        return match lang {
            Language::Uk => "щойно",
            Language::Ru => "только что",
            Language::En => "just now",
        }
        .to_string();
    } else if secs < 3600 {
        (secs / 60, 0)
    } else if secs < 86_400 {
        (secs / 3600, 1)
    } else {
        (secs / 86_400, 2)
    };
    match (lang, unit) {
        (Language::Uk, 0) => format!("{n} хв тому"),
        (Language::Uk, 1) => format!("{n} год тому"),
        (Language::Uk, _) => format!("{n} дн тому"),
        (Language::Ru, 0) => format!("{n} мин назад"),
        (Language::Ru, 1) => format!("{n} ч назад"),
        (Language::Ru, _) => format!("{n} дн назад"),
        (Language::En, 0) => format!("{n} min ago"),
        (Language::En, 1) => format!("{n} h ago"),
        (Language::En, _) => format!("{n} d ago"),
    }
}

fn format_date(unix_ms: u64) -> String {
    i64::try_from(unix_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".into())
}

/// "added 2026-11-23 · last seen 5 min ago" line of a device row.
pub fn device_meta(lang: Language, device: &Device, now_ms: u64) -> String {
    let added = format_date(device.paired_unix_ms);
    let seen = if device.last_seen_unix_ms == 0 {
        match lang {
            Language::Uk => "ще не підключався".to_string(),
            Language::Ru => "ещё не подключался".to_string(),
            Language::En => "never connected".to_string(),
        }
    } else {
        relative_time(lang, now_ms, device.last_seen_unix_ms)
    };
    match lang {
        Language::Uk => format!("додано {added} · востаннє {seen}"),
        Language::Ru => format!("добавлено {added} · в последний раз {seen}"),
        Language::En => format!("added {added} · last seen {seen}"),
    }
}

/// Horizontal runs of dark modules `(x, y, len)` of a row-major `width` x `width` matrix
/// (fewer draw calls than one square per module).
pub fn qr_runs(width: usize, dark: &[bool]) -> Vec<(usize, usize, usize)> {
    let mut runs = Vec::new();
    if width == 0 {
        return runs;
    }
    for (y, row) in dark.chunks(width).enumerate() {
        let mut x = 0;
        while x < row.len() {
            if row[x] {
                let start = x;
                while x < row.len() && row[x] {
                    x += 1;
                }
                runs.push((start, y, x - start));
            } else {
                x += 1;
            }
        }
    }
    runs
}

/// Whole-pixel module size and total panel side (quiet zone included) closest to `target_px`.
pub fn qr_layout(width: usize, target_px: usize) -> (usize, usize) {
    let modules = width + 2 * QR_QUIET;
    let module = (target_px / modules).max(1);
    (module, module * modules)
}

/// State of the "add phone" dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PairDialogState {
    #[default]
    Closed,
    /// Waiting for the gateway to hand out a code.
    Starting,
    ShowingQr { url: String, expires_ms: u64 },
    /// A phone scanned the code and waits for the user.
    Confirming { request: PairRequest },
    Done { allowed: bool },
    Expired,
    /// The gateway connection ended.
    GatewayStopped,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairEvent {
    /// The user asks for a (new) code.
    NewCode,
    Cancel,
    Started(PairingInfo),
    StartFailed(String),
    Request(PairRequest),
    Answered { allow: bool },
    AnswerFailed(String),
    /// Clock tick with the current unix time.
    Tick(u64),
    GatewayGone,
}

pub fn transition(state: PairDialogState, event: PairEvent) -> PairDialogState {
    use PairDialogState as S;
    use PairEvent as E;
    match (state, event) {
        (_, E::Cancel) => S::Closed,
        (S::Confirming { request }, E::NewCode) => S::Confirming { request },
        (_, E::NewCode) => S::Starting,
        (S::Closed, _) => S::Closed,
        (S::Starting | S::ShowingQr { .. }, E::Started(info)) => {
            S::ShowingQr { url: info.url, expires_ms: info.expires_unix_ms }
        }
        (S::Starting, E::StartFailed(e)) => S::Error(e),
        (S::ShowingQr { .. } | S::Confirming { .. }, E::Request(request)) => S::Confirming { request },
        (S::Confirming { .. }, E::Answered { allow }) => S::Done { allowed: allow },
        (S::Confirming { .. }, E::AnswerFailed(e)) => S::Error(e),
        (S::ShowingQr { expires_ms, .. }, E::Tick(now)) if now >= expires_ms => S::Expired,
        (S::Done { allowed }, E::GatewayGone) => S::Done { allowed },
        (_, E::GatewayGone) => S::GatewayStopped,
        (state, _) => state,
    }
}

// ----- runtime -----

enum PairCmd {
    Confirm { request_id: u64, allow: bool },
}

enum Msg {
    Gateway(GatewayStatus),
    Devices(Vec<Device>),
    DevicesError(String),
    Pair(PairEvent),
    #[cfg(windows)]
    Firewall(FirewallUi, FirewallNote),
}

struct PhoneRt {
    handle: Handle,
    base: String,
    tx: UnboundedSender<Msg>,
}
impl Global for PhoneRt {}

#[derive(Default)]
struct PhoneState {
    gateway: GatewayStatus,
    devices: Vec<Device>,
    devices_error: Option<String>,
    dialog: PairDialogState,
    cmd: Option<UnboundedSender<PairCmd>>,
    polling: bool,
    #[cfg(windows)]
    firewall: FirewallUi,
    #[cfg(windows)]
    firewall_note: FirewallNote,
}
impl Global for PhoneState {}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn apply(state: &mut PhoneState, msg: Msg) {
    match msg {
        Msg::Gateway(status) => {
            if matches!(status, GatewayStatus::NotRunning | GatewayStatus::Error(_)) {
                state.devices.clear();
            }
            state.gateway = status;
        }
        Msg::Devices(devices) => {
            state.devices = devices;
            state.devices_error = None;
        }
        Msg::DevicesError(error) => state.devices_error = Some(error),
        #[cfg(windows)]
        Msg::Firewall(ui, note) => {
            state.firewall = ui;
            state.firewall_note = note;
        }
        Msg::Pair(event) => {
            state.dialog = transition(std::mem::take(&mut state.dialog), event);
            if state.dialog == PairDialogState::Closed {
                state.cmd = None;
            }
        }
    }
}

/// Wires the runtime in and starts the gateway when the setting is on. Call once at GUI startup.
pub fn startup(handle: Handle, base: String, cx: &mut App) {
    let (tx, rx) = unbounded();
    cx.set_global(PhoneRt { handle, base, tx });
    cx.set_global(PhoneState::default());
    cx.spawn(async move |cx: &mut AsyncApp| drain(rx, cx).await).detach();
    if settings::get(cx).phone_sync {
        enable(cx);
    }
}

async fn drain(mut rx: UnboundedReceiver<Msg>, cx: &mut AsyncApp) {
    while let Some(msg) = rx.next().await {
        let answered = matches!(msg, Msg::Pair(PairEvent::Answered { .. }));
        let _ = cx.update(|cx| {
            apply(cx.default_global::<PhoneState>(), msg);
            if answered {
                refresh(cx);
            }
            cx.refresh_windows();
        });
    }
}

fn rt(cx: &App) -> Option<(Handle, String, UnboundedSender<Msg>)> {
    cx.try_global::<PhoneRt>().map(|r| (r.handle.clone(), r.base.clone(), r.tx.clone()))
}

/// Connects, reads status and device list.
async fn poll(base: &str, tx: &UnboundedSender<Msg>) {
    let mut client = match ControlClient::connect_as(base, sich_app::build_id()).await {
        Ok(client) => client,
        Err(_) => {
            let _ = tx.unbounded_send(Msg::Gateway(GatewayStatus::NotRunning));
            return;
        }
    };
    match client.status().await {
        Ok(s) => {
            let _ = tx.unbounded_send(Msg::Gateway(GatewayStatus::Running {
                endpoint_id: s.endpoint_id,
                connected: s.connected,
            }));
        }
        Err(e) => {
            let _ = tx.unbounded_send(Msg::Gateway(GatewayStatus::Error(format!("{e:#}"))));
            return;
        }
    }
    let _ = tx.unbounded_send(match client.list_devices().await {
        Ok(devices) => Msg::Devices(devices),
        Err(e) => Msg::DevicesError(format!("{e:#}")),
    });
}

/// Re-reads status and devices in the background.
pub fn refresh(cx: &mut App) {
    let Some((handle, base, tx)) = rt(cx) else { return };
    handle.spawn(async move { poll(&base, &tx).await });
}

/// Starts the gateway (if needed) and reads its state.
fn enable(cx: &mut App) {
    let Some((handle, base, tx)) = rt(cx) else { return };
    cx.default_global::<PhoneState>().gateway = GatewayStatus::Starting;
    cx.refresh_windows();
    handle.spawn(async move {
        match crate::cli::ensure_gateway(&base).await {
            Ok(()) => poll(&base, &tx).await,
            Err(e) => {
                let _ = tx.unbounded_send(Msg::Gateway(GatewayStatus::Error(format!("{e:#}"))));
            }
        }
    });
}

/// Stops the gateway.
fn disable(cx: &mut App) {
    let Some((handle, base, tx)) = rt(cx) else { return };
    handle.spawn(async move {
        let msg = match ControlClient::connect_as(&base, sich_app::build_id()).await {
            Ok(mut client) => match client.shutdown().await {
                Ok(()) => Msg::Gateway(GatewayStatus::NotRunning),
                Err(e) => Msg::Gateway(GatewayStatus::Error(format!("{e:#}"))),
            },
            Err(_) => Msg::Gateway(GatewayStatus::NotRunning),
        };
        let _ = tx.unbounded_send(msg);
    });
}

/// The settings switch.
pub fn set_enabled(enabled: bool, cx: &mut App) {
    settings::set_phone_sync(enabled, cx);
    if enabled {
        enable(cx);
    } else {
        disable(cx);
    }
}

fn revoke(endpoint_id: String, cx: &mut App) {
    let Some((handle, base, tx)) = rt(cx) else { return };
    handle.spawn(async move {
        match ControlClient::connect_as(&base, sich_app::build_id()).await {
            Ok(mut client) => {
                if let Err(e) = client.revoke(&endpoint_id).await {
                    let _ = tx.unbounded_send(Msg::DevicesError(format!("{e:#}")));
                    return;
                }
            }
            Err(e) => {
                let _ = tx.unbounded_send(Msg::DevicesError(format!("{e:#}")));
                return;
            }
        }
        poll(&base, &tx).await;
    });
}

/// Reads whether the firewall rule exists (off the UI thread; `netsh`, no elevation).
#[cfg(windows)]
fn check_firewall(cx: &mut App) {
    let Some((handle, _, tx)) = rt(cx) else { return };
    if cx.default_global::<PhoneState>().firewall == FirewallUi::Busy {
        return;
    }
    handle.spawn_blocking(move || {
        let ui = if crate::phone_firewall::is_present(crate::phone_firewall::rule_port()) {
            FirewallUi::Allowed
        } else {
            FirewallUi::Blocked
        };
        let _ = tx.unbounded_send(Msg::Firewall(ui, FirewallNote::None));
    });
}

/// Adds or removes the rule through one UAC prompt, then re-checks.
#[cfg(windows)]
fn change_firewall(action: crate::phone_firewall::Action, cx: &mut App) {
    let Some((handle, _, tx)) = rt(cx) else { return };
    let state = cx.default_global::<PhoneState>();
    if state.firewall == FirewallUi::Busy {
        return;
    }
    state.firewall = FirewallUi::Busy;
    state.firewall_note = FirewallNote::None;
    cx.refresh_windows();
    handle.spawn_blocking(move || {
        let port = crate::phone_firewall::rule_port();
        let outcome = crate::phone_firewall::run_elevated(action, port);
        let (ui, note) = firewall_after(outcome, crate::phone_firewall::is_present(port));
        let _ = tx.unbounded_send(Msg::Firewall(ui, note));
    });
}

/// Called when the settings dialog opens: refreshes now and every 3 s while any dialog is open.
pub fn on_settings_open(window: &Window, cx: &mut App) {
    refresh(cx);
    #[cfg(windows)]
    check_firewall(cx);
    if cx.default_global::<PhoneState>().polling {
        return;
    }
    cx.default_global::<PhoneState>().polling = true;
    let handle = window.window_handle();
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let open = handle.update(cx, |_, window, cx| window.has_active_dialog(cx)).unwrap_or(false);
            if !open {
                break;
            }
            let _ = cx.update(refresh);
        }
        let _ = cx.update(|cx| cx.default_global::<PhoneState>().polling = false);
    })
    .detach();
}

async fn pair_worker(base: String, mut cmds: UnboundedReceiver<PairCmd>, tx: UnboundedSender<Msg>) {
    let send = |event: PairEvent| {
        let _ = tx.unbounded_send(Msg::Pair(event));
    };
    let mut client = match ControlClient::connect_as(&base, sich_app::build_id()).await {
        Ok(client) => client,
        Err(e) => return send(PairEvent::StartFailed(format!("{e:#}"))),
    };
    match client.start_pairing().await {
        Ok(info) => send(PairEvent::Started(info)),
        Err(e) => return send(PairEvent::StartFailed(format!("{e:#}"))),
    }
    loop {
        tokio::select! {
            request = client.next_pair_request() => match request {
                Ok(Some(request)) => send(PairEvent::Request(request)),
                Ok(None) | Err(_) => return send(PairEvent::GatewayGone),
            },
            cmd = cmds.next() => match cmd {
                Some(PairCmd::Confirm { request_id, allow }) => match client.confirm(request_id, allow).await {
                    Ok(()) => send(PairEvent::Answered { allow }),
                    Err(e) => send(PairEvent::AnswerFailed(format!("{e:#}"))),
                },
                // The dialog closed: dropping the client ends this pairing round.
                None => return,
            },
        }
    }
}

/// Starts a pairing round (a fresh gateway connection); the previous one, if any, is dropped.
fn begin_pairing(cx: &mut App) {
    let Some((handle, base, tx)) = rt(cx) else { return };
    let (cmd_tx, cmd_rx) = unbounded();
    let state = cx.default_global::<PhoneState>();
    state.cmd = Some(cmd_tx);
    state.dialog = transition(std::mem::take(&mut state.dialog), PairEvent::NewCode);
    handle.spawn(pair_worker(base, cmd_rx, tx));
    cx.refresh_windows();
}

fn end_pairing(cx: &mut App) {
    apply(cx.default_global::<PhoneState>(), Msg::Pair(PairEvent::Cancel));
}

fn answer(allow: bool, cx: &mut App) {
    let state = cx.default_global::<PhoneState>();
    if let (PairDialogState::Confirming { request }, Some(cmd)) = (&state.dialog, &state.cmd) {
        let _ = cmd.unbounded_send(PairCmd::Confirm { request_id: request.request_id, allow });
    }
}

/// Opens the "add phone" dialog and starts pairing.
pub fn open_pair_dialog(window: &mut Window, cx: &mut App) {
    if rt(cx).is_none() {
        return;
    }
    begin_pairing(cx);
    // One tick per second: countdown and code expiry.
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = cx.update(|cx| {
                    let state = cx.default_global::<PhoneState>();
                    if state.dialog == PairDialogState::Closed {
                        return false;
                    }
                    apply(state, Msg::Pair(PairEvent::Tick(now_ms())));
                    cx.refresh_windows();
                    true
                });
            if !alive {
                break;
            }
        }
    })
    .detach();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let lang = settings::language(cx);
        let state = cx.try_global::<PhoneState>().map(|s| s.dialog.clone()).unwrap_or_default();
        let theme = cx.theme();
        let text = |t: String| div().text_sm().child(t);
        let mut body = v_flex().gap_3().items_center();
        let mut footer = DialogFooter::new();
        let cancel = |id: &'static str, label: Tr| {
            Button::new(id).label(tr(lang, label)).on_click(|_, window, cx| window.close_dialog(cx))
        };
        let new_code = Button::new("phone-new-code")
            .primary()
            .label(tr(lang, Tr::PhoneNewCode))
            .on_click(|_, _, cx| begin_pairing(cx));
        match &state {
            PairDialogState::Closed => {}
            PairDialogState::Starting => {
                body = body.child(div().text_sm().text_color(theme.muted_foreground).child(tr(lang, Tr::PhoneGettingCode)));
                footer = footer.child(cancel("phone-cancel", Tr::Cancel));
            }
            PairDialogState::ShowingQr { url, expires_ms } => {
                let left = expires_ms.saturating_sub(now_ms());
                let url = url.clone();
                body = body
                    .child(qr_element(&url))
                    .child(text(tr(lang, Tr::PhoneScanQr).to_string()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(valid_for_text(lang, left)));
                footer = footer
                    .child(
                        Button::new("phone-copy")
                            .label(tr(lang, Tr::PhoneCopyLink))
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))),
                    )
                    .child(cancel("phone-cancel", Tr::Cancel));
            }
            PairDialogState::Confirming { request } => {
                body = body.child(text(allow_text(lang, &request.device_name, &request.endpoint_id)));
                footer = footer
                    .child(Button::new("phone-deny").label(tr(lang, Tr::PhoneDeny)).on_click(|_, _, cx| answer(false, cx)))
                    .child(
                        Button::new("phone-allow")
                            .primary()
                            .label(tr(lang, Tr::PhoneAllow))
                            .on_click(|_, _, cx| answer(true, cx)),
                    );
            }
            PairDialogState::Done { allowed } => {
                let key = if *allowed { Tr::PhoneAdded } else { Tr::PhoneDenied };
                body = body.child(text(tr(lang, key).to_string()));
                footer = footer.child(cancel("phone-close", Tr::Close));
            }
            PairDialogState::Expired => {
                body = body.child(text(tr(lang, Tr::PhoneCodeExpired).to_string()));
                footer = footer.child(cancel("phone-cancel", Tr::Cancel)).child(new_code);
            }
            PairDialogState::GatewayStopped => {
                body = body.child(div().text_sm().text_color(theme.danger).child(tr(lang, Tr::PhoneNotRunning)));
                footer = footer.child(cancel("phone-close", Tr::Close)).child(new_code);
            }
            PairDialogState::Error(message) => {
                body = body.child(div().text_sm().text_color(theme.danger).child(message.clone()));
                footer = footer.child(cancel("phone-close", Tr::Close)).child(new_code);
            }
        }
        dialog
            .title(tr(lang, Tr::PhoneAdd))
            .w(px(420.))
            .on_close(|_, _, cx| end_pairing(cx))
            .child(body)
            .footer(footer)
    });
}

fn qr_element(url: &str) -> AnyElement {
    let Ok(code) = qrcode::QrCode::new(url.as_bytes()) else { return div().into_any_element() };
    let width = code.width();
    let dark: Vec<bool> = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    let (module, total) = qr_layout(width, QR_TARGET_PX);
    let mut panel = div().relative().w(px(total as f32)).h(px(total as f32)).rounded_lg().bg(rgb(0xffffff));
    for (x, y, len) in qr_runs(width, &dark) {
        panel = panel.child(
            div()
                .absolute()
                .left(px(((x + QR_QUIET) * module) as f32))
                .top(px(((y + QR_QUIET) * module) as f32))
                .w(px((len * module) as f32))
                .h(px(module as f32))
                .bg(rgb(0x000000)),
        );
    }
    panel.into_any_element()
}

// ----- settings section -----

/// Body of the "Phone" section.
pub fn section(lang: Language, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let prefs = settings::get(cx);
    let (gateway, devices, devices_error) = match cx.try_global::<PhoneState>() {
        Some(s) => (s.gateway.clone(), s.devices.clone(), s.devices_error.clone()),
        None => (GatewayStatus::Unknown, Vec::new(), None),
    };
    let (status, is_error) = status_line(lang, &if prefs.phone_sync { gateway.clone() } else { GatewayStatus::NotRunning });
    let running = prefs.phone_sync && matches!(gateway, GatewayStatus::Running { .. });
    let now = now_ms();

    let mut list = v_flex().gap_1p5();
    if devices.is_empty() {
        list = list.child(div().text_xs().text_color(theme.muted_foreground).child(tr(lang, Tr::PhoneNoDevices)));
    }
    for (i, device) in devices.iter().enumerate() {
        let id = device.endpoint_id.clone();
        list = list.child(
            h_flex()
                .gap_3()
                .items_center()
                .justify_between()
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(div().text_sm().child(format!("{} · {}", device.name, short_id(&device.endpoint_id))))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(device_meta(lang, device, now)),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("phone-revoke-{i}")))
                        .xsmall()
                        .label(tr(lang, Tr::PhoneRevoke))
                        .on_click(move |_, _, cx| revoke(id.clone(), cx)),
                ),
        );
    }

    #[cfg(windows)]
    let firewall = {
        let (fw, note) = match cx.try_global::<PhoneState>() {
            Some(s) => (s.firewall, s.firewall_note.clone()),
            None => (FirewallUi::Unknown, FirewallNote::None),
        };
        let (text, fw_error) = firewall_line(lang, fw, &note);
        let button = match fw {
            FirewallUi::Allowed => Some(
                Button::new("phone-fw-remove")
                    .xsmall()
                    .link()
                    .label(tr(lang, Tr::PhoneFwRemove))
                    .on_click(|_, _, cx| change_firewall(crate::phone_firewall::Action::Remove, cx)),
            ),
            FirewallUi::Blocked => Some(
                Button::new("phone-fw-add")
                    .xsmall()
                    .primary()
                    .label(tr(lang, Tr::PhoneFwAdd))
                    .on_click(|_, _, cx| change_firewall(crate::phone_firewall::Action::Add, cx)),
            ),
            FirewallUi::Unknown | FirewallUi::Busy => None,
        };
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().text_xs().text_color(if fw_error { theme.danger } else { theme.muted_foreground }).child(text))
                    .children(button),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(tr(lang, Tr::PhoneFwHint)))
    };
    #[cfg(not(windows))]
    let firewall = div();

    v_flex()
        .gap_1p5()
        .child(
            Switch::new("phone-sync")
                .checked(prefs.phone_sync)
                .label(tr(lang, Tr::PhoneSync))
                .on_click(|checked, _, cx| set_enabled(*checked, cx)),
        )
        .child(div().text_xs().text_color(theme.muted_foreground).child(tr(lang, Tr::PhoneSyncDesc)))
        .child(div().text_xs().text_color(if is_error { theme.danger } else { theme.muted_foreground }).child(status))
        .child(firewall)
        .child(
            h_flex().child(
                Button::new("phone-add")
                    .xsmall()
                    .label(tr(lang, Tr::PhoneAdd))
                    .disabled(!running)
                    .on_click(|_, window, cx| open_pair_dialog(window, cx)),
            ),
        )
        .children(devices_error.map(|e| div().text_xs().text_color(theme.danger).child(e)))
        .child(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(expires: u64) -> PairingInfo {
        PairingInfo { url: "sich://pair?x".into(), expires_unix_ms: expires }
    }

    fn request() -> PairRequest {
        PairRequest { request_id: 3, device_name: "Pixel".into(), endpoint_id: "abcdefghijklmnop".into() }
    }

    fn showing() -> PairDialogState {
        PairDialogState::ShowingQr { url: "sich://pair?x".into(), expires_ms: 1000 }
    }

    #[test]
    fn happy_path_to_done() {
        use PairDialogState as S;
        use PairEvent as E;
        let s = transition(S::Closed, E::NewCode);
        assert_eq!(s, S::Starting);
        let s = transition(s, E::Started(info(1000)));
        assert_eq!(s, showing());
        let s = transition(s, E::Tick(999));
        assert_eq!(s, showing());
        let s = transition(s, E::Request(request()));
        assert_eq!(s, S::Confirming { request: request() });
        let s = transition(s, E::Answered { allow: true });
        assert_eq!(s, S::Done { allowed: true });
        assert_eq!(transition(S::Confirming { request: request() }, E::Answered { allow: false }), S::Done { allowed: false });
    }

    #[test]
    fn expiry_and_new_code() {
        use PairDialogState as S;
        use PairEvent as E;
        assert_eq!(transition(showing(), E::Tick(1000)), S::Expired);
        assert_eq!(transition(S::Expired, E::NewCode), S::Starting);
        // A request in flight is never dropped by the clock or by "new code".
        let confirming = S::Confirming { request: request() };
        assert_eq!(transition(confirming.clone(), E::Tick(10_000)), confirming);
        assert_eq!(transition(confirming.clone(), E::NewCode), confirming);
        // Ticks outside the QR stage change nothing.
        assert_eq!(transition(S::Starting, E::Tick(10_000)), S::Starting);
        assert_eq!(transition(S::Closed, E::Tick(10_000)), S::Closed);
    }

    #[test]
    fn errors_cancel_and_gateway_loss() {
        use PairDialogState as S;
        use PairEvent as E;
        assert_eq!(transition(S::Starting, E::StartFailed("x".into())), S::Error("x".into()));
        assert_eq!(transition(S::Error("x".into()), E::NewCode), S::Starting);
        assert_eq!(
            transition(S::Confirming { request: request() }, E::AnswerFailed("late".into())),
            S::Error("late".into())
        );
        assert_eq!(transition(showing(), E::GatewayGone), S::GatewayStopped);
        assert_eq!(transition(S::GatewayStopped, E::NewCode), S::Starting);
        assert_eq!(transition(S::Done { allowed: true }, E::GatewayGone), S::Done { allowed: true });
        for s in [S::Starting, showing(), S::Expired, S::Done { allowed: false }, S::Error("e".into())] {
            assert_eq!(transition(s, E::Cancel), S::Closed);
        }
    }

    #[test]
    fn stale_events_are_ignored() {
        use PairDialogState as S;
        use PairEvent as E;
        // Events of a dropped round must not reopen a closed dialog or skip stages.
        assert_eq!(transition(S::Closed, E::Started(info(5))), S::Closed);
        assert_eq!(transition(S::Closed, E::Request(request())), S::Closed);
        assert_eq!(transition(S::Closed, E::GatewayGone), S::Closed);
        assert_eq!(transition(S::Starting, E::Request(request())), S::Starting);
        assert_eq!(transition(S::Expired, E::Request(request())), S::Expired);
        assert_eq!(transition(showing(), E::Answered { allow: true }), showing());
    }

    #[test]
    fn qr_runs_merge_horizontal_neighbours() {
        // 3x3:  X X .   / . X X / X . X
        let dark = [true, true, false, false, true, true, true, false, true];
        assert_eq!(qr_runs(3, &dark), vec![(0, 0, 2), (1, 1, 2), (0, 2, 1), (2, 2, 1)]);
        assert!(qr_runs(3, &[false; 9]).is_empty());
        assert_eq!(qr_runs(2, &[true; 4]), vec![(0, 0, 2), (0, 1, 2)]);
        assert!(qr_runs(0, &[]).is_empty());
    }

    #[test]
    fn qr_layout_uses_whole_pixel_modules() {
        // 33 modules + 2*4 quiet = 41 -> 5 px per module = 205 px.
        assert_eq!(qr_layout(33, 240), (5, 205));
        let (module, total) = qr_layout(500, 240);
        assert_eq!((module, total), (1, 508));
        let (module, total) = qr_layout(21, 240);
        assert_eq!(total, module * 29);
        assert!(total <= 240);
    }

    #[test]
    fn real_pair_url_gives_runs_inside_the_matrix() {
        let url = "sich://pair?v=1&id=abc&addrs=1.2.3.4%3A5&relay=&secret=xyz&host=PC";
        let code = qrcode::QrCode::new(url.as_bytes()).unwrap();
        let dark: Vec<bool> = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
        let runs = qr_runs(code.width(), &dark);
        assert!(!runs.is_empty());
        assert!(runs.iter().all(|&(x, y, len)| len > 0 && x + len <= code.width() && y < code.width()));
        assert_eq!(runs.iter().map(|r| r.2).sum::<usize>(), dark.iter().filter(|d| **d).count());
    }

    #[test]
    fn status_lines() {
        let running = GatewayStatus::Running { endpoint_id: "0123456789abcdef".into(), connected: 2 };
        assert_eq!(status_line(Language::Uk, &running), ("Працює · ID 0123456789 · Підключено: 2".to_string(), false));
        assert!(status_line(Language::En, &running).0.contains("ID 0123456789 "));
        assert_eq!(status_line(Language::Uk, &GatewayStatus::NotRunning), ("Не запущено".to_string(), false));
        assert_eq!(status_line(Language::En, &GatewayStatus::Error("boom".into())), ("boom".to_string(), true));
        for lang in Language::ALL {
            assert!(!status_line(lang, &GatewayStatus::Starting).0.is_empty());
        }
    }

    #[test]
    fn countdown_and_texts() {
        assert_eq!(format_countdown(581_000), "9:41");
        assert_eq!(format_countdown(0), "0:00");
        assert_eq!(format_countdown(59_001), "1:00");
        assert_eq!(valid_for_text(Language::Uk, 581_000), "Дійсний ще 9:41");
        assert_eq!(allow_text(Language::Uk, "Pixel", "0123456789abcdef"), "Дозволити «Pixel» (0123456789)?");
        for lang in Language::ALL {
            assert!(allow_text(lang, "N", "abc").contains('N'));
            assert!(valid_for_text(lang, 1000).contains("0:01"));
        }
    }

    #[test]
    fn relative_times() {
        let now = 10_000_000_000;
        assert_eq!(relative_time(Language::En, now, now - 30_000), "just now");
        assert_eq!(relative_time(Language::En, now, now + 5_000), "just now");
        assert_eq!(relative_time(Language::En, now, now - 5 * 60_000), "5 min ago");
        assert_eq!(relative_time(Language::Uk, now, now - 3 * 3_600_000), "3 год тому");
        assert_eq!(relative_time(Language::Ru, now, now - 2 * 86_400_000), "2 дн назад");
    }

    #[test]
    fn device_meta_handles_never_seen() {
        let d = Device { endpoint_id: "e".into(), name: "n".into(), paired_unix_ms: 1_700_000_000_000, last_seen_unix_ms: 0 };
        let text = device_meta(Language::En, &d, 1_700_000_100_000);
        assert!(text.starts_with("added 20"), "{text}");
        assert!(text.ends_with("never connected"), "{text}");
        let seen = Device { last_seen_unix_ms: 1_700_000_000_000, ..d };
        assert!(device_meta(Language::En, &seen, 1_700_000_300_000).ends_with("last seen 5 min ago"));
    }

    #[cfg(windows)]
    #[test]
    fn firewall_lines_and_outcomes() {
        use crate::phone_firewall::Outcome;
        let none = FirewallNote::None;
        assert_eq!(
            firewall_line(Language::Uk, FirewallUi::Allowed, &none),
            ("Пряме підключення в локальній мережі: дозволено ✓".to_string(), false)
        );
        assert!(firewall_line(Language::Uk, FirewallUi::Blocked, &none).0.ends_with("заблоковано брандмауером"));
        assert!(firewall_line(Language::En, FirewallUi::Busy, &none).0.ends_with("…"));
        assert!(firewall_line(Language::Uk, FirewallUi::Blocked, &FirewallNote::Cancelled).0.ends_with("Скасовано"));
        let (text, is_error) = firewall_line(Language::En, FirewallUi::Blocked, &FirewallNote::Error("boom".into()));
        assert!(is_error && text.ends_with("boom"));
        assert_eq!(firewall_after(Outcome::Done, true), (FirewallUi::Allowed, FirewallNote::None));
        assert_eq!(firewall_after(Outcome::Cancelled, false), (FirewallUi::Blocked, FirewallNote::Cancelled));
        assert_eq!(
            firewall_after(Outcome::Failed("x".into()), false),
            (FirewallUi::Blocked, FirewallNote::Error("x".into()))
        );
    }

    #[test]
    fn apply_updates_state() {
        let mut state = PhoneState::default();
        let dev = Device { endpoint_id: "e".into(), name: "n".into(), paired_unix_ms: 1, last_seen_unix_ms: 2 };
        apply(&mut state, Msg::Devices(vec![dev]));
        assert_eq!(state.devices.len(), 1);
        apply(&mut state, Msg::DevicesError("x".into()));
        assert_eq!(state.devices_error.as_deref(), Some("x"));
        apply(&mut state, Msg::Gateway(GatewayStatus::NotRunning));
        assert!(state.devices.is_empty());
        apply(&mut state, Msg::Pair(PairEvent::NewCode));
        assert_eq!(state.dialog, PairDialogState::Starting);
        apply(&mut state, Msg::Pair(PairEvent::Cancel));
        assert_eq!(state.dialog, PairDialogState::Closed);
        assert!(state.cmd.is_none());
    }
}
