//! End-to-end tests: a real daemon, the real gateway, and an in-process iroh endpoint playing
//! the phone. Everything is isolated: unique daemon base name, temp config/data dirs, no relay.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, SecretKey, TransportAddr};
use sich_gateway::{ControlClient, Opts};
use sich_proto::{AgentKind, ClientMsg, PROTO_VERSION, ServerMsg};
use sich_remote_proto::{
    ALPN, MIN_COMPATIBLE, GatewayMsg, PairUrl, PhoneMsg, REMOTE_PROTO_VERSION, RejectReason, TermRef, Workspace, decode_frame, encode_frame, frame_codec,
};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::codec::{Framed, FramedRead, FramedWrite, LengthDelimitedCodec};

const WAIT: Duration = Duration::from_secs(15);

fn isolate() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    static CONFIG: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
    CONFIG.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: set once, before any daemon of this test binary starts.
        unsafe {
            std::env::set_var("SICH_CONFIG_DIR", dir.path().join("config"));
            std::env::set_var("SICH_DATA_DIR", dir.path().join("data"));
            std::env::set_var("SICH_WORKTREE_ROOT", dir.path().join("worktrees"));
            std::env::set_var("SICH_GATEWAY_RELAY", "disabled");
            std::env::set_var("SICH_GATEWAY_PORT", "0");
            std::env::set_var("CLAUDE_CONFIG_DIR", dir.path().join("claude"));
            std::env::set_var("CODEX_HOME", dir.path().join("codex"));
        }
        dir
    });
}

// ----- harness -----

struct Harness {
    base: String,
    daemon_socket: String,
    phone_dir: TempDir,
    upload_dir: TempDir,
    daemon: JoinHandle<anyhow::Result<()>>,
    gateway: Option<JoinHandle<anyhow::Result<()>>>,
    port: u16,
}

impl Harness {
    async fn start() -> Harness {
        Self::start_with(Duration::from_secs(120)).await
    }

    async fn start_with(confirm_timeout: Duration) -> Harness {
        Self::start_full(confirm_timeout, 0).await
    }

    async fn start_full(confirm_timeout: Duration, port: u16) -> Harness {
        isolate();
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = format!("sich-gw-test-{}-{n}", std::process::id());
        let build = format!("gwb{n}x{}", std::process::id());
        let daemon = tokio::spawn({
            let (base, build) = (base.clone(), build.clone());
            async move { sich_daemon::run_generation(&base, &build).await }
        });
        let mut h = Harness {
            daemon_socket: sich_daemon::generation::socket(&base, &build),
            base,
            phone_dir: tempfile::tempdir().unwrap(),
            upload_dir: tempfile::tempdir().unwrap(),
            daemon,
            gateway: None,
            port,
        };
        let opts = h.opts(false, "build-one", confirm_timeout);
        h.gateway = Some(tokio::spawn(sich_gateway::run(opts)));
        // Ready when the control pipe answers.
        drop(h.control().await);
        h
    }

    fn opts(&self, takeover: bool, build_id: &str, confirm_timeout: Duration) -> Opts {
        Opts {
            base: self.base.clone(),
            takeover,
            build_id: build_id.into(),
            phone_dir: self.phone_dir.path().to_path_buf(),
            upload_dir: self.upload_dir.path().to_path_buf(),
            relay_disabled: true,
            port: self.port,
            host_name: "TEST-HOST".into(),
            confirm_timeout,
        }
    }

    async fn control(&self) -> ControlClient {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match ControlClient::connect_as(&self.base, "test").await {
                Ok(c) => return c,
                Err(e) if tokio::time::Instant::now() > deadline => panic!("gateway control pipe never came up: {e:#}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    async fn daemon_conn(&self) -> Framed<sich_daemon::ClientStream, LengthDelimitedCodec> {
        let deadline = tokio::time::Instant::now() + WAIT;
        let stream = loop {
            match sich_daemon::connect(&self.daemon_socket).await {
                Ok(s) => break s,
                Err(e) if tokio::time::Instant::now() > deadline => panic!("daemon never came up: {e}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };
        let mut conn = Framed::new(stream, sich_proto::frame_codec());
        daemon_send(&mut conn, ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "gateway-test".into() }).await;
        assert!(matches!(daemon_recv(&mut conn).await, ServerMsg::Welcome { .. }));
        conn
    }

    async fn create_terminal(&self, shell: &str, args: &[&str], cols: u16, rows: u16) -> TermRef {
        let mut conn = self.daemon_conn().await;
        daemon_send(
            &mut conn,
            ClientMsg::CreateTerminal {
                cwd: None,
                shell: Some(shell.into()),
                args: args.iter().map(|a| a.to_string()).collect(),
                cols,
                rows,
                worktree: None,
            },
        )
        .await;
        loop {
            if let ServerMsg::TerminalCreated(info) = daemon_recv(&mut conn).await {
                return TermRef { daemon: self.daemon_socket.clone(), id: info.id };
            }
        }
    }

    async fn interactive_shell(&self) -> TermRef {
        if cfg!(windows) {
            self.create_terminal("cmd.exe", &[], 80, 24).await
        } else {
            self.create_terminal("/bin/sh", &[], 80, 24).await
        }
    }

    async fn daemon_size(&self, term: &TermRef) -> (u16, u16) {
        let mut conn = self.daemon_conn().await;
        daemon_send(&mut conn, ClientMsg::ListTerminals).await;
        loop {
            if let ServerMsg::Terminals(list) = daemon_recv(&mut conn).await {
                let t = list.iter().find(|t| t.id == term.id).expect("terminal listed");
                return (t.cols, t.rows);
            }
        }
    }

    async fn wait_daemon_size(&self, term: &TermRef, want: (u16, u16)) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let got = self.daemon_size(term).await;
            if got == want {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "daemon size stayed {got:?}, wanted {want:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Pairs a phone through the control channel acting as the desktop user.
    async fn pair(&self, key: &SecretKey, name: &str, allow: bool) -> Phone {
        let mut control = self.control().await;
        let info = control.start_pairing().await.unwrap();
        let url = PairUrl::parse(&info.url).expect("valid pair url");
        assert_eq!(url.host, "TEST-HOST");
        let desktop = tokio::spawn(async move {
            let request = control.next_pair_request().await.unwrap().expect("a pair request");
            control.confirm(request.request_id, allow).await.unwrap();
            (request, control)
        });
        let mut phone = Phone::connect(key.clone(), &url.endpoint_id, &url.addrs).await;
        phone.hello(name, Some(url.secret.clone())).await;
        assert_eq!(phone.recv().await, GatewayMsg::PairPending);
        let (request, _control) = desktop.await.unwrap();
        assert_eq!(request.device_name, name);
        assert_eq!(request.endpoint_id, key.public().to_string());
        phone
    }

    /// Pairs and returns the connected, welcomed phone.
    async fn paired_phone(&self, name: &str) -> (Phone, SecretKey) {
        let key = SecretKey::generate();
        let mut phone = self.pair(&key, name, true).await;
        assert!(matches!(phone.recv().await, GatewayMsg::Welcome { .. }));
        (phone, key)
    }

    async fn connect_known(&self, key: &SecretKey) -> Phone {
        let mut control = self.control().await;
        let status = control.status().await.unwrap();
        Phone::connect(key.clone(), &status.endpoint_id, &status.addrs).await
    }

    async fn stop(mut self) {
        if let Ok(mut c) = ControlClient::connect(&self.base).await {
            let _ = c.shutdown().await;
        }
        if let Some(g) = self.gateway.take() {
            let _ = timeout(WAIT, g).await;
        }
        if let Ok(mut c) = sich_daemon::connect(&self.daemon_socket).await.map(|s| Framed::new(s, sich_proto::frame_codec())) {
            daemon_send(&mut c, ClientMsg::Shutdown).await;
        }
        let _ = timeout(WAIT, &mut self.daemon).await;
    }
}

async fn daemon_send(c: &mut Framed<sich_daemon::ClientStream, LengthDelimitedCodec>, msg: ClientMsg) {
    c.send(sich_proto::encode(&msg).unwrap()).await.unwrap();
}

async fn daemon_recv(c: &mut Framed<sich_daemon::ClientStream, LengthDelimitedCodec>) -> ServerMsg {
    let frame = timeout(WAIT, c.next()).await.expect("daemon timeout").expect("daemon closed").expect("read frame");
    sich_proto::decode(&frame).unwrap()
}

// ----- the phone -----

struct Phone {
    _endpoint: Endpoint,
    conn: Connection,
    reader: FramedRead<RecvStream, LengthDelimitedCodec>,
    writer: FramedWrite<SendStream, LengthDelimitedCodec>,
    /// Terminal that gets an automatic cursor-position reply (ConPTY waits for one).
    dsr: Option<TermRef>,
}

impl Phone {
    async fn connect(key: SecretKey, id: &str, addrs: &[String]) -> Phone {
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let addr = EndpointAddr::from_parts(
            id.parse().expect("endpoint id"),
            addrs.iter().map(|a| TransportAddr::Ip(a.parse().expect("socket addr"))),
        );
        let conn = timeout(WAIT, endpoint.connect(addr, ALPN)).await.expect("connect timeout").expect("connect");
        let (send, recv) = conn.open_bi().await.unwrap();
        Phone {
            _endpoint: endpoint,
            conn,
            reader: FramedRead::new(recv, frame_codec()),
            writer: FramedWrite::new(send, frame_codec()),
            dsr: None,
        }
    }

    async fn send(&mut self, msg: PhoneMsg) {
        self.writer.send(encode_frame(&msg).unwrap()).await.unwrap();
    }

    async fn hello(&mut self, name: &str, secret: Option<String>) {
        self.send(PhoneMsg::Hello {
            proto_version: REMOTE_PROTO_VERSION,
            min_compatible: MIN_COMPATIBLE,
            app_version: "test".into(),
            device_name: name.into(),
            capabilities: vec![],
            pair_secret: secret,
        })
        .await;
    }

    /// Next message, `None` when the gateway closed the stream.
    async fn recv_opt(&mut self) -> Option<GatewayMsg> {
        loop {
            let frame = timeout(WAIT, self.reader.next()).await.expect("timed out waiting for the gateway");
            let msg: GatewayMsg = match frame {
                Some(Ok(f)) => decode_frame(&f).unwrap(),
                Some(Err(_)) | None => return None,
            };
            if let (GatewayMsg::Output { bytes, .. }, Some(term)) = (&msg, self.dsr.clone()) {
                if bytes.windows(3).any(|w| w == b"[6n") {
                    self.send(PhoneMsg::Input { term, bytes: b"\x1b[1;1R".to_vec() }).await;
                }
            }
            return Some(msg);
        }
    }

    async fn recv(&mut self) -> GatewayMsg {
        self.recv_opt().await.expect("the gateway closed the stream")
    }

    async fn until<T>(&mut self, mut pick: impl FnMut(&GatewayMsg) -> Option<T>) -> T {
        loop {
            let msg = self.recv().await;
            if let Some(v) = pick(&msg) {
                return v;
            }
        }
    }

    async fn workspace_with(&mut self, term: &TermRef) -> Workspace {
        self.until(|m| match m {
            GatewayMsg::Workspace(ws) if ws.terminals.iter().any(|t| &t.term == term) => Some(ws.clone()),
            _ => None,
        })
        .await
    }

    /// Output of `term` since its last Reset until it contains `needle`.
    async fn output_until(&mut self, term: &TermRef, needle: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        loop {
            match self.recv().await {
                GatewayMsg::Reset { term: t, .. } if &t == term => buf.clear(),
                GatewayMsg::Output { term: t, bytes, .. } if &t == term => {
                    buf.extend_from_slice(&bytes);
                    if contains(&buf, needle) {
                        return buf;
                    }
                }
                _ => {}
            }
        }
    }

    async fn subscribe(&mut self, term: &TermRef, cols: u16, rows: u16, fit: bool) {
        self.dsr = Some(term.clone());
        self.send(PhoneMsg::Subscribe { term: term.clone(), cols, rows, fit, replay_bytes: 1 << 20 }).await;
    }

    /// Expects `Rejected{reason}` followed by the end of the stream.
    async fn expect_rejected(&mut self, reason: RejectReason) {
        loop {
            match self.recv().await {
                GatewayMsg::Rejected { reason: got } => {
                    assert_eq!(got, reason);
                    break;
                }
                GatewayMsg::PairPending | GatewayMsg::Workspace(_) | GatewayMsg::Pong { .. } => {}
                other => panic!("expected Rejected({reason:?}), got {other:?}"),
            }
        }
        assert!(self.recv_opt().await.is_none(), "the gateway closes after Rejected");
        self.conn.close(0u32.into(), b"done");
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&[0u8; 32]);
    v
}

fn devices_json(h: &Harness) -> String {
    std::fs::read_to_string(h.phone_dir.path().join("devices.json")).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_the_requested_port() {
    let port = std::net::UdpSocket::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
    let h = Harness::start_full(Duration::from_secs(120), port).await;
    let status = h.control().await.status().await.unwrap();
    assert_eq!(status.port, port);
    h.stop().await;
}

// ----- pairing -----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpaired_phone_is_rejected() {
    let h = Harness::start().await;
    let mut phone = h.connect_known(&SecretKey::generate()).await;
    phone.hello("Stranger", None).await;
    phone.expect_rejected(RejectReason::Unpaired).await;
    assert!(!devices_json(&h).contains("Stranger"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_secret_is_rejected_and_the_real_one_still_works() {
    let h = Harness::start().await;
    let mut control = h.control().await;
    let info = control.start_pairing().await.unwrap();
    let url = PairUrl::parse(&info.url).unwrap();

    let mut bad = h.connect_known(&SecretKey::generate()).await;
    bad.hello("Mallory", Some("not-the-secret".into())).await;
    bad.expect_rejected(RejectReason::PairSecretInvalid).await;

    // One failure does not burn the secret.
    let mut good = h.connect_known(&SecretKey::generate()).await;
    good.hello("Alice", Some(url.secret.clone())).await;
    assert_eq!(good.recv().await, GatewayMsg::PairPending);
    let request = control.next_pair_request().await.unwrap().unwrap();
    assert_eq!(request.device_name, "Alice");
    control.confirm(request.request_id, true).await.unwrap();
    assert!(matches!(good.recv().await, GatewayMsg::Welcome { .. }));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_is_one_shot() {
    let h = Harness::start().await;
    let mut control = h.control().await;
    let url = PairUrl::parse(&control.start_pairing().await.unwrap().url).unwrap();
    let mut first = h.connect_known(&SecretKey::generate()).await;
    first.hello("First", Some(url.secret.clone())).await;
    assert_eq!(first.recv().await, GatewayMsg::PairPending);
    let request = control.next_pair_request().await.unwrap().unwrap();
    control.confirm(request.request_id, true).await.unwrap();
    assert!(matches!(first.recv().await, GatewayMsg::Welcome { .. }));

    let mut second = h.connect_known(&SecretKey::generate()).await;
    second.hello("Second", Some(url.secret)).await;
    second.expect_rejected(RejectReason::PairSecretInvalid).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_allow_welcomes_persists_and_survives_reconnect() {
    let h = Harness::start().await;
    let key = SecretKey::generate();
    let mut phone = h.pair(&key, "Pixel 8", true).await;
    match phone.recv().await {
        GatewayMsg::Welcome { proto_version, host_name, .. } => {
            assert_eq!(proto_version, REMOTE_PROTO_VERSION);
            assert_eq!(host_name, "TEST-HOST");
        }
        other => panic!("expected Welcome, got {other:?}"),
    }
    assert!(matches!(phone.recv().await, GatewayMsg::Workspace(ws) if ws.host_name == "TEST-HOST"));

    let json = devices_json(&h);
    assert!(json.contains(&key.public().to_string()) && json.contains("Pixel 8"), "{json}");
    let mut control = h.control().await;
    let devices = control.list_devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "Pixel 8");
    assert_eq!(control.status().await.unwrap().connected, 1);

    // A later connection needs no secret.
    phone.conn.close(0u32.into(), b"bye");
    let mut again = h.connect_known(&key).await;
    again.hello("Pixel 8", None).await;
    assert!(matches!(again.recv().await, GatewayMsg::Welcome { .. }));
    assert!(matches!(again.recv().await, GatewayMsg::Workspace(_)));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_deny_rejects_and_stores_nothing() {
    let h = Harness::start().await;
    let key = SecretKey::generate();
    let mut phone = h.pair(&key, "Denied", false).await;
    phone.expect_rejected(RejectReason::PairDenied).await;
    assert!(!devices_json(&h).contains("Denied"));
    let mut retry = h.connect_known(&key).await;
    retry.hello("Denied", None).await;
    retry.expect_rejected(RejectReason::Unpaired).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unanswered_pairing_times_out() {
    let h = Harness::start_with(Duration::from_millis(400)).await;
    let mut control = h.control().await;
    let url = PairUrl::parse(&control.start_pairing().await.unwrap().url).unwrap();
    let mut phone = h.connect_known(&SecretKey::generate()).await;
    phone.hello("Slowpoke", Some(url.secret)).await;
    assert_eq!(phone.recv().await, GatewayMsg::PairPending);
    let request = control.next_pair_request().await.unwrap().unwrap();
    phone.expect_rejected(RejectReason::PairTimeout).await;
    assert!(control.confirm(request.request_id, true).await.is_err(), "a late answer is refused");
    assert!(!devices_json(&h).contains("Slowpoke"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompatible_protocol_is_rejected() {
    let h = Harness::start().await;
    let mut phone = h.connect_known(&SecretKey::generate()).await;
    phone
        .send(PhoneMsg::Hello {
            proto_version: 7,
            min_compatible: 5,
            app_version: "future".into(),
            device_name: "Future".into(),
            capabilities: vec![],
            pair_secret: None,
        })
        .await;
    phone.expect_rejected(RejectReason::Incompatible { gateway_version: REMOTE_PROTO_VERSION, gateway_min: MIN_COMPATIBLE }).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_drops_the_live_connection() {
    let h = Harness::start().await;
    let (mut phone, key) = h.paired_phone("Doomed").await;
    let devices = h.control().await.list_devices().await.unwrap();
    assert!(h.control().await.revoke(&devices[0].endpoint_id).await.unwrap());
    phone.expect_rejected(RejectReason::Revoked).await;
    assert!(h.control().await.list_devices().await.unwrap().is_empty());
    assert!(!h.control().await.revoke("nope").await.unwrap());

    let mut again = h.connect_known(&key).await;
    again.hello("Doomed", None).await;
    again.expect_rejected(RejectReason::Unpaired).await;
    h.stop().await;
}

// ----- workspace and terminals -----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_lists_terminals_created_through_the_daemon() {
    let h = Harness::start().await;
    let (mut phone, _) = h.paired_phone("Lister").await;
    let term = h.interactive_shell().await;
    let ws = phone.workspace_with(&term).await;
    let t = ws.terminals.iter().find(|t| t.term == term).unwrap();
    assert_eq!((t.cols, t.rows), (80, 24));
    assert!(t.exited.is_none() && t.agent.is_none());
    assert_eq!(ws.host_name, "TEST-HOST");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_replays_history_and_reports_exit() {
    let h = Harness::start().await;
    let (shell, args): (&str, &[&str]) =
        if cfg!(windows) { ("cmd.exe", &["/c", "echo SICH_REPLAY_MARK"]) } else { ("/bin/sh", &["-c", "echo SICH_REPLAY_MARK"]) };
    let term = h.create_terminal(shell, args, 80, 24).await;
    let (mut phone, _) = h.paired_phone("Replayer").await;
    phone.workspace_with(&term).await;
    phone.subscribe(&term, 80, 24, false).await;
    let out = phone.output_until(&term, b"SICH_REPLAY_MARK").await;
    assert!(contains(&out, b"SICH_REPLAY_MARK"));
    phone.until(|m| matches!(m, GatewayMsg::Exited { .. }).then_some(())).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_sends_reset_first_then_live_output_reaches_a_second_phone_too() {
    let h = Harness::start().await;
    let term = h.interactive_shell().await;
    let (mut a, _) = h.paired_phone("A").await;
    let (mut b, _) = h.paired_phone("B").await;
    a.subscribe(&term, 80, 24, false).await;
    b.subscribe(&term, 80, 24, false).await;
    // The first terminal message after Subscribe is the Reset.
    let reset = a.until(|m| matches!(m, GatewayMsg::Reset { .. } | GatewayMsg::Output { .. }).then(|| m.clone())).await;
    assert_eq!(reset, GatewayMsg::Reset { term: term.clone(), cols: 80, rows: 24 });
    let (calc, answer) = if cfg!(windows) { ("set /a 1234*5\r", b"6170".as_slice()) } else { ("echo $((1234*5))\n", b"6170".as_slice()) };
    a.send(PhoneMsg::Input { term: term.clone(), bytes: calc.as_bytes().to_vec() }).await;
    let out_a = a.output_until(&term, answer).await;
    let out_b = b.output_until(&term, answer).await;
    assert!(contains(&out_a, answer) && contains(&out_b, answer));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_text_without_agent_is_raw_and_submits() {
    let h = Harness::start().await;
    let term = h.interactive_shell().await;
    let (mut phone, _) = h.paired_phone("Composer").await;
    phone.workspace_with(&term).await;
    phone.subscribe(&term, 80, 24, false).await;
    let text = if cfg!(windows) { "set /a 2468*5" } else { "echo $((2468*5))" };
    phone.send(PhoneMsg::SendText { term: term.clone(), text: text.into(), images: vec![], submit: true }).await;
    // 12340 only appears when the command really ran (the echo of the typed text has no result).
    let out = phone.output_until(&term, b"12340").await;
    assert!(!contains(&out, b"\x1b[200~"), "plain shells get no bracketed paste");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_saves_the_image_and_send_text_includes_its_path() {
    let h = Harness::start().await;
    let term = h.interactive_shell().await;
    let (mut phone, _) = h.paired_phone("Uploader").await;
    phone.workspace_with(&term).await;
    phone.subscribe(&term, 80, 24, false).await;

    phone.send(PhoneMsg::UploadImage { req_id: 1, file_name: "evil.exe".into(), data: b"MZ not an image".to_vec() }).await;
    let rejected = phone.until(|m| matches!(m, GatewayMsg::Uploaded { req_id: 1, .. }).then(|| m.clone())).await;
    assert!(matches!(rejected, GatewayMsg::Uploaded { result: Err(_), .. }));

    phone.send(PhoneMsg::UploadImage { req_id: 2, file_name: "shot.png".into(), data: png() }).await;
    let id = phone
        .until(|m| match m {
            GatewayMsg::Uploaded { req_id: 2, result } => Some(result.clone().expect("png accepted")),
            _ => None,
        })
        .await;
    assert!(id.starts_with("phone-") && id.ends_with(".png"), "{id}");
    let saved = h.upload_dir.path().join(&id);
    assert_eq!(std::fs::read(&saved).unwrap(), png());

    phone.send(PhoneMsg::SendText { term: term.clone(), text: "look".into(), images: vec![id.clone()], submit: false }).await;
    // The path is typed into the terminal (the shell echoes it).
    phone.output_until(&term, id.as_bytes()).await;

    phone.send(PhoneMsg::SendText { term: term.clone(), text: "x".into(), images: vec!["phone-0-0.png".into()], submit: false }).await;
    phone.until(|m| matches!(m, GatewayMsg::Error { message } if message.contains("unknown image")).then_some(())).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fit_resizes_the_pty_and_unsubscribe_restores_the_desktop_size() {
    let h = Harness::start().await;
    let term = h.interactive_shell().await;
    let (mut phone, _) = h.paired_phone("Fitter").await;
    phone.workspace_with(&term).await;
    phone.subscribe(&term, 50, 20, true).await;
    let size = phone.until(|m| matches!(m, GatewayMsg::TermSize { .. }).then(|| m.clone())).await;
    assert_eq!(size, GatewayMsg::TermSize { term: term.clone(), cols: 50, rows: 20, fit: true });
    h.wait_daemon_size(&term, (50, 20)).await;

    phone.send(PhoneMsg::Resize { term: term.clone(), cols: 60, rows: 25 }).await;
    h.wait_daemon_size(&term, (60, 25)).await;

    phone.send(PhoneMsg::Unsubscribe { term: term.clone() }).await;
    h.wait_daemon_size(&term, (80, 24)).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_fit_toggles_and_disconnect_restores() {
    let h = Harness::start().await;
    let term = h.interactive_shell().await;
    let (mut phone, _) = h.paired_phone("Toggler").await;
    phone.workspace_with(&term).await;
    phone.subscribe(&term, 70, 22, false).await;
    phone.until(|m| matches!(m, GatewayMsg::TermSize { fit: false, .. }).then_some(())).await;
    assert_eq!(h.daemon_size(&term).await, (80, 24), "not fitting: the PTY keeps the desktop size");

    phone.send(PhoneMsg::SetFit { term: term.clone(), fit: true }).await;
    h.wait_daemon_size(&term, (70, 22)).await;
    phone.send(PhoneMsg::SetFit { term: term.clone(), fit: false }).await;
    h.wait_daemon_size(&term, (80, 24)).await;

    phone.send(PhoneMsg::SetFit { term: term.clone(), fit: true }).await;
    h.wait_daemon_size(&term, (70, 22)).await;
    phone.conn.close(0u32.into(), b"gone");
    h.wait_daemon_size(&term, (80, 24)).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_is_answered() {
    let h = Harness::start().await;
    let (mut phone, _) = h.paired_phone("Pinger").await;
    phone.send(PhoneMsg::Ping { nonce: 42 }).await;
    phone.until(|m| matches!(m, GatewayMsg::Pong { nonce: 42 }).then_some(())).await;
    h.stop().await;
}

// ----- single instance -----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_gateway_exits_and_takeover_replaces_the_first() {
    let h = Harness::start().await;
    let status = h.control().await.status().await.unwrap();
    assert_eq!(status.build_id, "build-one");

    // Same base without --takeover: returns right away, the first keeps running.
    timeout(WAIT, sich_gateway::run(h.opts(false, "build-two", Duration::from_secs(1)))).await.unwrap().unwrap();
    assert_eq!(h.control().await.status().await.unwrap().build_id, "build-one");

    // With takeover the old one stops and the new one answers.
    let mut h = h;
    let second = tokio::spawn(sich_gateway::run(h.opts(true, "build-two", Duration::from_secs(1))));
    let first = h.gateway.take().unwrap();
    timeout(WAIT, first).await.expect("the old gateway stopped").unwrap().unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Ok(mut c) = ControlClient::connect(&h.base).await {
            if c.gateway_build_id() == "build-two" {
                // Same identity: the key lives in the shared phone dir.
                assert_eq!(c.status().await.unwrap().endpoint_id, status.endpoint_id);
                break;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "takeover never completed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.gateway = Some(second);
    h.stop().await;
}

// Not on Windows: ConPTY swallows APC sequences before the daemon sees them (the conversion
// itself is covered by the `kitty_to_iterm` and `stream` unit tests).
#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kitty_graphics_reach_the_phone_as_an_iterm_inline_image() {
    use base64::Engine as _;
    let h = Harness::start().await;
    let dir = TempDir::new().unwrap();
    let image = png();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&image);
    let mut data = b"KITTY_BEGIN\n\x1b_Ga=t,f=100,i=7,U=1,q=2;".to_vec();
    data.extend_from_slice(b64.as_bytes());
    data.extend_from_slice(b"\x1b\\\x1b_Ga=p,U=1,i=7,c=4,r=2\x1b\\\x1b_Ga=T,f=100;");
    data.extend_from_slice(b64.as_bytes());
    data.extend_from_slice(b"\x1b\\\nKITTY_END\n");
    let file = dir.path().join("kitty.bin");
    std::fs::write(&file, &data).unwrap();
    let file = file.to_string_lossy().into_owned();
    let (shell, args): (&str, Vec<String>) = ("/bin/sh", vec!["-c".into(), format!("cat '{file}'")]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let term = h.create_terminal(shell, &args, 80, 24).await;
    let (mut phone, _) = h.paired_phone("Imager").await;
    phone.workspace_with(&term).await;
    phone.subscribe(&term, 80, 24, false).await;
    let out = phone.output_until(&term, b"KITTY_END").await;
    let want = format!("\x1b]1337;File=inline=1;size={};preserveAspectRatio=1:{b64}\x07", image.len());
    assert!(contains(&out, want.as_bytes()), "iTerm2 sequence missing in {:?}", String::from_utf8_lossy(&out));
    let normalized = format!("\x1b_Ga=t,f=100,t=d,q=2,i=7,m=0;{b64}\x1b\\\x1b_Ga=p,U=1,q=2,i=7,c=4,r=2\x1b\\");
    assert!(contains(&out, normalized.as_bytes()), "normalized APC missing in {:?}", String::from_utf8_lossy(&out));
    assert!(!contains(&out, b"i=7,U=1"), "original control data must not leak");
    h.stop().await;
}

// ----- chat view -----

fn claude_transcript_path(session: &str) -> std::path::PathBuf {
    // The terminal's cwd is not known to the test: the gateway finds the file by scanning buckets.
    std::path::PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap()).join("projects").join("some-bucket").join(format!("{session}.jsonl"))
}

fn user_line(n: u32, text: &str) -> String {
    format!(r#"{{"type":"user","uuid":"u{n}","message":{{"role":"user","content":"{text}"}}}}"#)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_snapshot_live_update_paging_and_image_over_the_phone_link() {
    use base64::Engine as _;
    use sich_remote_proto::{ChatBlock, ChatStatus};
    use std::io::Write as _;

    let h = Harness::start().await;
    let term = h.interactive_shell().await;
    let session = format!("chat-it-{}", std::process::id());
    // Give the terminal a Claude session id the official way: a hook event.
    let mut daemon = h.daemon_conn().await;
    let payload = format!(r#"{{"session_id":"{session}","cwd":"C:\\dev","hook_event_name":"SessionStart"}}"#);
    daemon_send(&mut daemon, ClientMsg::AgentHook { term_id: term.id, agent: AgentKind::Claude, payload }).await;

    let key = SecretKey::generate();
    let mut phone = h.pair(&key, "Chatter", true).await;
    let GatewayMsg::Welcome { capabilities, .. } = phone.recv().await else { panic!("Welcome first") };
    assert!(capabilities.iter().any(|c| c == "chat"), "{capabilities:?}");
    phone.until(|m| matches!(m, GatewayMsg::Workspace(ws) if ws.terminals.iter().any(|t| t.term == term && t.agent.is_some())).then_some(())).await;

    // No transcript yet: Pending.
    phone.send(PhoneMsg::ChatSubscribe { term: term.clone(), limit: 2 }).await;
    let status = phone
        .until(|m| match m {
            GatewayMsg::ChatSnapshot { term: t, status, .. } if t == &term => Some(status.clone()),
            _ => None,
        })
        .await;
    assert_eq!(status, ChatStatus::Pending);

    // The file appears: a fresh Ready snapshot with the last `limit` messages.
    let path = claude_transcript_path(&session);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut png_bytes = Vec::new();
    ::image::DynamicImage::new_rgb8(6, 3).write_to(&mut std::io::Cursor::new(&mut png_bytes), ::image::ImageFormat::Png).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    let image_line = format!(
        r#"{{"type":"user","uuid":"img","message":{{"content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{b64}"}}}}]}}}}"#
    );
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "{}\n{}\n{}", user_line(0, "first"), user_line(1, "second"), image_line).unwrap();
    f.flush().unwrap();
    let (messages, has_more) = phone
        .until(|m| match m {
            GatewayMsg::ChatSnapshot { term: t, status: ChatStatus::Ready, messages, has_more } if t == &term => Some((messages.clone(), *has_more)),
            _ => None,
        })
        .await;
    assert_eq!(messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["u1", "img"]);
    assert!(has_more);

    // Live update.
    writeln!(f, "{}", user_line(2, "third")).unwrap();
    f.flush().unwrap();
    let update = phone
        .until(|m| match m {
            GatewayMsg::ChatUpdate { term: t, messages } if t == &term => Some(messages.clone()),
            _ => None,
        })
        .await;
    assert_eq!(update.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["u2"]);

    // Paging back.
    phone.send(PhoneMsg::ChatLoadEarlier { term: term.clone(), before_order: messages[0].order, limit: 5 }).await;
    let (earlier, more) = phone
        .until(|m| match m {
            GatewayMsg::ChatEarlier { term: t, messages, has_more } if t == &term => Some((messages.clone(), *has_more)),
            _ => None,
        })
        .await;
    assert_eq!(earlier.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["u0"]);
    assert!(!more);

    // Image by ref, and an unknown ref.
    let ChatBlock::Image { image_ref, .. } = messages[1].blocks[0].clone() else { panic!("image block") };
    phone.send(PhoneMsg::ChatImage { req_id: 7, term: term.clone(), image_ref }).await;
    phone.send(PhoneMsg::ChatImage { req_id: 8, term: term.clone(), image_ref: "i1.9".into() }).await;
    let mut results = std::collections::HashMap::new();
    while results.len() < 2 {
        if let GatewayMsg::ChatImageData { req_id, result } = phone.recv().await {
            results.insert(req_id, result);
        }
    }
    let ok = results[&7].as_ref().expect("image bytes");
    assert_eq!(ok.mime, "image/png");
    assert_eq!(::image::load_from_memory(&ok.data).unwrap().width(), 6);
    assert!(results[&8].is_err());

    // After unsubscribing, image requests for the closed chat fail.
    phone.send(PhoneMsg::ChatUnsubscribe { term: term.clone() }).await;
    phone.send(PhoneMsg::ChatImage { req_id: 9, term: term.clone(), image_ref: "i0.0".into() }).await;
    let closed = phone
        .until(|m| match m {
            GatewayMsg::ChatImageData { req_id: 9, result } => Some(result.clone()),
            _ => None,
        })
        .await;
    assert!(closed.is_err());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_for_a_terminal_without_an_agent_is_unavailable() {
    use sich_remote_proto::ChatStatus;
    let h = Harness::start().await;
    let term = h.interactive_shell().await;
    let (mut phone, _) = h.paired_phone("NoAgent").await;
    phone.workspace_with(&term).await;
    phone.send(PhoneMsg::ChatSubscribe { term: term.clone(), limit: 10 }).await;
    let status = phone
        .until(|m| match m {
            GatewayMsg::ChatSnapshot { status, .. } => Some(status.clone()),
            _ => None,
        })
        .await;
    assert!(matches!(status, ChatStatus::Unavailable { .. }), "{status:?}");
    // A terminal that does not exist is an error, not a hang.
    let ghost = TermRef { daemon: term.daemon.clone(), id: term.id + 1000 };
    phone.send(PhoneMsg::ChatSubscribe { term: ghost, limit: 10 }).await;
    phone.until(|m| matches!(m, GatewayMsg::Error { .. }).then_some(())).await;
    h.stop().await;
}
