//! Sich phone gateway: a daemon client (like the GUI) that serves paired phones over iroh.
//! See `docs/phone-mvp.md`.

pub mod compose;
pub mod control;
pub mod convert;
pub mod fit;
pub mod kitty_to_iterm;
mod hub;
pub mod outbox;
pub mod pairing;
mod phone;
pub mod pool;
pub mod store;
pub mod stream;
pub mod uploads;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use interprocess::local_socket::ListenerOptions;
use iroh::endpoint::{BindOpts, QuicTransportConfig, presets};
use iroh::{Endpoint, SecretKey};
use tokio_util::sync::CancellationToken;

pub use control::{ControlClient, PairRequest, PairingInfo, Status};
pub use hub::{filter_addrs, sanitize_name};
pub use store::Device;

/// QUIC keep-alive and idle timeout from the spec.
const KEEP_ALIVE: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const TAKEOVER_WAIT: Duration = Duration::from_secs(5);

/// Fixed UDP port of the gateway, so one firewall rule keeps working across builds.
pub const DEFAULT_PORT: u16 = 6769;

/// Port requested through `SICH_GATEWAY_PORT` (`0` = random); unset or invalid = `DEFAULT_PORT`.
pub fn port_from_env(value: Option<&str>) -> u16 {
    value.and_then(|v| v.trim().parse::<u16>().ok()).unwrap_or(DEFAULT_PORT)
}

/// Ports to try in order: the requested one, then a random one (`0`) when it is busy.
pub fn ports_to_try(requested: u16) -> Vec<u16> {
    if requested == 0 { vec![0] } else { vec![requested, 0] }
}

/// The port the endpoint is reachable on: the IPv4 socket's, else the first one; `0` if none.
pub fn pick_port(sockets: &[SocketAddr]) -> u16 {
    sockets.iter().find(|a| a.is_ipv4()).or_else(|| sockets.first()).map_or(0, |a| a.port())
}

pub struct Opts {
    /// Daemon base name (`sich_daemon::socket_name_from_env()`); the control pipe is `<base>-gateway`.
    pub base: String,
    /// Replace a gateway of another build that is already running.
    pub takeover: bool,
    pub build_id: String,
    /// `<config>/sich/phone`: `gateway.key` and `devices.json`.
    pub phone_dir: PathBuf,
    pub upload_dir: PathBuf,
    /// No relay and no address discovery (tests, LAN only).
    pub relay_disabled: bool,
    /// Requested UDP port (`0` = random). A busy port falls back to a random one.
    pub port: u16,
    pub host_name: String,
    /// How long the desktop user has to confirm a pair request.
    pub confirm_timeout: Duration,
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("SICH_CONFIG_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(dirs::config_dir)
}

fn computer_name() -> String {
    ["COMPUTERNAME", "HOSTNAME", "HOST"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "Computer".to_string())
}

impl Opts {
    /// Settings for the real gateway: environment overrides (`SICH_SOCKET`, `SICH_CONFIG_DIR`,
    /// `SICH_GATEWAY_RELAY=disabled`) and the default locations.
    pub fn from_env(base: String, takeover: bool, build_id: &str) -> anyhow::Result<Opts> {
        Ok(Opts {
            base,
            takeover,
            build_id: build_id.to_string(),
            phone_dir: config_dir().context("no config dir")?.join("sich").join("phone"),
            upload_dir: uploads::default_dir(),
            relay_disabled: std::env::var("SICH_GATEWAY_RELAY").is_ok_and(|v| v.eq_ignore_ascii_case("disabled")),
            port: port_from_env(std::env::var("SICH_GATEWAY_PORT").ok().as_deref()),
            host_name: computer_name(),
            confirm_timeout: pairing::CONFIRM_TIMEOUT,
        })
    }
}

/// Starts file logging to `<data_local_dir>/sich/logs/gateway.log`. Keep the guard alive.
pub fn init_logging() -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = sich_daemon::data_local_dir().context("no local data dir")?.join("sich").join("logs");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let appender = tracing_appender::rolling::never(&dir, "gateway.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init()
        .map_err(|e| anyhow::anyhow!("init logging: {e}"))?;
    Ok(guard)
}

async fn bind_endpoint(opts: &Opts, key: [u8; 32]) -> anyhow::Result<Endpoint> {
    let mut last_err = None;
    for port in ports_to_try(opts.port) {
        match bind_endpoint_on(opts, key, port).await {
            Ok(endpoint) => return Ok(endpoint),
            Err(e) => {
                tracing::warn!(port, "could not bind the gateway port: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no port to bind")))
}

async fn bind_endpoint_on(opts: &Opts, key: [u8; 32], port: u16) -> anyhow::Result<Endpoint> {
    let transport = QuicTransportConfig::builder()
        .keep_alive_interval(KEEP_ALIVE)
        .max_idle_timeout(Some(IDLE_TIMEOUT.try_into().context("idle timeout")?))
        .build();
    let builder = if opts.relay_disabled { Endpoint::builder(presets::Minimal) } else { Endpoint::builder(presets::N0) };
    builder
        .secret_key(SecretKey::from_bytes(&key))
        .alpns(vec![sich_remote_proto::ALPN.to_vec()])
        .transport_config(transport)
        .clear_ip_transports()
        .bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))
        .context("IPv4 bind address")?
        // IPv6 may be unavailable or its port taken: not required.
        .bind_addr_with_opts(SocketAddr::from(([0u16; 8], port)), BindOpts::default().set_is_required(false))
        .context("IPv6 bind address")?
        .bind()
        .await
        .context("bind iroh endpoint")
}

/// Waits until nothing answers on the control pipe any more.
async fn wait_gone(base: &str) -> bool {
    let deadline = tokio::time::Instant::now() + TAKEOVER_WAIT;
    while tokio::time::Instant::now() < deadline {
        if ControlClient::connect(base).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Runs the gateway until a control client sends `Shutdown`. Returns immediately (Ok) when a
/// gateway for the same base is already running and `takeover` is off.
pub async fn run(opts: Opts) -> anyhow::Result<()> {
    if let Ok(mut running) = ControlClient::connect(&opts.base).await {
        if !opts.takeover {
            tracing::info!("a gateway is already running, exiting");
            return Ok(());
        }
        tracing::info!(old = running.gateway_build_id(), "taking over from the running gateway");
        running.shutdown().await.ok();
        drop(running);
        anyhow::ensure!(wait_gone(&opts.base).await, "the running gateway did not stop within {} s", TAKEOVER_WAIT.as_secs());
    }
    let pipe = control::pipe_name(&opts.base);
    let listener = match ListenerOptions::new().name(control::to_name(&pipe)?).create_tokio() {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!("another gateway is already running (control pipe `{pipe}` is in use)")
        }
        Err(e) => return Err(e).with_context(|| format!("bind control pipe `{pipe}`")),
    };

    let key = store::load_or_create_key(&opts.phone_dir)?;
    let store = store::Store::open(&opts.phone_dir);
    let upload_dir = opts.upload_dir.clone();
    tokio::task::spawn_blocking(move || {
        uploads::prune(&upload_dir, std::time::SystemTime::now(), uploads::MAX_AGE);
    });
    let endpoint = bind_endpoint(&opts, key).await?;
    tracing::info!(id = %endpoint.id(), base = %opts.base, port = pick_port(&endpoint.bound_sockets()), relay_disabled = opts.relay_disabled, "gateway started");

    let shutdown = CancellationToken::new();
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (pool, changed) = pool::Pool::start(opts.base.clone(), events_tx);
    let hub = hub::Hub::new(
        hub::HubConfig {
            host_name: opts.host_name.clone(),
            build_id: opts.build_id.clone(),
            upload_dir: opts.upload_dir.clone(),
            relay_disabled: opts.relay_disabled,
            confirm_timeout: opts.confirm_timeout,
        },
        endpoint.clone(),
        pool.clone(),
        store,
        changed,
        shutdown.clone(),
    );
    tokio::spawn(hub.clone().run_background(events_rx));
    tokio::spawn(control::serve(listener, hub.clone()));

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            incoming = endpoint.accept() => match incoming {
                Some(incoming) => {
                    tokio::spawn(phone::handle(hub.clone(), incoming));
                }
                None => break,
            },
        }
    }
    tracing::info!("gateway shutting down");
    shutdown.cancel();
    pool.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(3), endpoint.close()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_from_env_defaults_and_overrides() {
        assert_eq!(port_from_env(None), DEFAULT_PORT);
        assert_eq!(port_from_env(Some("")), DEFAULT_PORT);
        assert_eq!(port_from_env(Some("junk")), DEFAULT_PORT);
        assert_eq!(port_from_env(Some("70000")), DEFAULT_PORT);
        assert_eq!(port_from_env(Some("0")), 0);
        assert_eq!(port_from_env(Some(" 7000 ")), 7000);
    }

    #[test]
    fn busy_fixed_port_falls_back_to_random() {
        assert_eq!(ports_to_try(6769), vec![6769, 0]);
        assert_eq!(ports_to_try(0), vec![0]);
    }

    #[test]
    fn pick_port_prefers_ipv4() {
        let v4: SocketAddr = "0.0.0.0:6769".parse().unwrap();
        let v6: SocketAddr = "[::]:7000".parse().unwrap();
        assert_eq!(pick_port(&[v6, v4]), 6769);
        assert_eq!(pick_port(&[v6]), 7000);
        assert_eq!(pick_port(&[]), 0);
    }
}
