//! ConnectionManager tests against an in-process fake gateway over a real iroh connection
//! (loopback, relay and discovery disabled).

use super::*;
use iroh::endpoint::{RecvStream, SendStream};
use sich_remote_proto::RejectReason;

struct TestUi {
    states: Mutex<Vec<ConnState>>,
    hosts_changed: Mutex<u32>,
}

impl TestUi {
    fn new() -> Arc<Self> {
        Arc::new(Self { states: Mutex::new(vec![]), hosts_changed: Mutex::new(0) })
    }
    fn states(&self) -> Vec<ConnState> {
        self.states.lock().unwrap().clone()
    }
}

impl Ui for TestUi {
    fn connection(&self, state: &ConnState) {
        self.states.lock().unwrap().push(state.clone());
    }
    fn workspace(&self, _: &Workspace) {}
    fn term_size(&self, _: &TermRef, _: u16, _: u16, _: bool) {}
    fn term_exited(&self, _: &TermRef, _: Option<i32>) {}
    fn hosts_changed(&self) {
        *self.hosts_changed.lock().unwrap() += 1;
    }
    fn gateway_error(&self, _: &str) {}
}

#[derive(Default)]
struct TestSink {
    events: Mutex<Vec<String>>,
}

impl OutputSink for TestSink {
    fn reset(&self, cols: u16, rows: u16) {
        self.events.lock().unwrap().push(format!("reset {cols}x{rows}"));
    }
    fn output(&self, bytes: Vec<u8>) {
        self.events.lock().unwrap().push(format!("out {}", String::from_utf8_lossy(&bytes)));
    }
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn term(id: u64) -> TermRef {
    TermRef { daemon: "d".into(), id }
}

async fn gw_endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .unwrap()
}

fn gw_addrs(ep: &Endpoint) -> Vec<String> {
    ep.bound_sockets().iter().filter(|a| a.is_ipv4()).map(|a| format!("127.0.0.1:{}", a.port())).collect()
}

struct GwConn {
    conn: Connection,
    send: FramedWrite<SendStream, tokio_util::codec::LengthDelimitedCodec>,
    recv: FramedRead<RecvStream, tokio_util::codec::LengthDelimitedCodec>,
}

impl GwConn {
    async fn accept(ep: &Endpoint) -> GwConn {
        let incoming = ep.accept().await.expect("endpoint closed");
        let conn = incoming.accept().unwrap().await.unwrap();
        let (send, recv) = conn.accept_bi().await.unwrap();
        GwConn {
            conn,
            send: FramedWrite::new(send, frame_codec()),
            recv: FramedRead::new(recv, frame_codec()),
        }
    }
    async fn recv(&mut self) -> PhoneMsg {
        let frame = tokio::time::timeout(Duration::from_secs(10), self.recv.next())
            .await
            .expect("gateway recv timed out")
            .expect("stream ended")
            .unwrap();
        decode_frame(&frame).unwrap()
    }
    /// Next message that is not a Ping.
    async fn recv_msg(&mut self) -> PhoneMsg {
        loop {
            let m = self.recv().await;
            if !matches!(m, PhoneMsg::Ping { .. }) {
                return m;
            }
        }
    }
    async fn send(&mut self, m: GatewayMsg) {
        self.send.send(encode_frame(&m).unwrap()).await.unwrap();
    }
    async fn welcome(&mut self, name: &str) {
        self.send(GatewayMsg::Welcome {
            proto_version: REMOTE_PROTO_VERSION,
            min_compatible: MIN_COMPATIBLE,
            host_name: name.into(),
            gateway_version: "test".into(),
            capabilities: vec![],
        })
        .await;
    }
}

struct Rig {
    mgr: ConnectionManager,
    ui: Arc<TestUi>,
    store: Arc<Mutex<HostStore>>,
    gw: Endpoint,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(HostStore::load(dir.path().join("hosts.json"))));
    let ui = TestUi::new();
    let mgr = ConnectionManager::start(
        ConnConfig { relay_disabled: true, device_name: "test phone".into() },
        ui.clone(),
        store.clone(),
        SecretKey::generate(),
        &tokio::runtime::Handle::current(),
    );
    Rig { mgr, ui, store, gw: gw_endpoint().await, _dir: dir }
}

impl Rig {
    fn host(&self) -> Host {
        Host {
            id: self.gw.id().to_string(),
            name: "My PC".into(),
            endpoint_id: self.gw.id().to_string(),
            addrs: gw_addrs(&self.gw),
            relay: None,
            paired_unix_ms: 1,
        }
    }
    fn pair_url(&self, secret: &str) -> PairUrl {
        PairUrl {
            endpoint_id: self.gw.id().to_string(),
            addrs: gw_addrs(&self.gw),
            relay: None,
            secret: secret.into(),
            host: "Desk".into(),
        }
    }
}

#[test]
fn endpoint_addr_parsing() {
    let ep = SecretKey::generate().public().to_string();
    let a = to_endpoint_addr(
        &ep,
        &["10.0.0.1:5000".into(), "garbage".into(), "[::1]:7".into()],
        Some("https://relay.example.com/"),
    )
    .unwrap();
    assert_eq!(a.ip_addrs().count(), 2);
    assert_eq!(a.relay_urls().count(), 1);
    // empty relay string = no relay
    assert_eq!(to_endpoint_addr(&ep, &[], Some("")).unwrap().relay_urls().count(), 0);
    assert!(to_endpoint_addr("not-a-key", &[], None).is_err());
    assert!(to_endpoint_addr(&ep, &[], Some("::: bad")).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_stores_host_and_connects() {
    let r = rig().await;
    r.mgr.pair(r.pair_url("sekret"));
    let mut g = GwConn::accept(&r.gw).await;
    match g.recv().await {
        PhoneMsg::Hello { pair_secret, device_name, proto_version, .. } => {
            assert_eq!(pair_secret.as_deref(), Some("sekret"));
            assert_eq!(device_name, "test phone");
            assert_eq!(proto_version, REMOTE_PROTO_VERSION);
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    g.send(GatewayMsg::PairPending).await;
    wait_for("pair_pending", || r.ui.states().contains(&ConnState::PairPending)).await;
    g.welcome("Desk PC").await;
    g.send(GatewayMsg::Workspace(Workspace {
        host_name: "Desk PC".into(),
        groups: vec![],
        projects: vec![],
        terminals: vec![],
    }))
    .await;
    wait_for("connected", || r.mgr.state() == ConnState::Connected { host_name: "Desk PC".into() }).await;
    wait_for("workspace", || r.mgr.workspace().is_some()).await;
    assert_eq!(
        r.ui.states()[..3],
        [ConnState::Connecting, ConnState::PairPending, ConnState::Connected { host_name: "Desk PC".into() }]
    );
    let store = r.store.lock().unwrap();
    assert_eq!(store.hosts().len(), 1);
    assert_eq!(store.hosts()[0].name, "Desk PC");
    assert_eq!(store.hosts()[0].endpoint_id, r.gw.id().to_string());
    assert_eq!(store.active_id(), Some(r.gw.id().to_string().as_str()));
    assert!(*r.ui.hosts_changed.lock().unwrap() >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_pairing_is_rejected_and_not_stored() {
    let r = rig().await;
    r.mgr.pair(r.pair_url("s"));
    let mut g = GwConn::accept(&r.gw).await;
    g.recv().await;
    g.send(GatewayMsg::Rejected { reason: RejectReason::PairDenied }).await;
    wait_for("rejected", || {
        matches!(r.mgr.state(), ConnState::Rejected { ref reason, .. } if reason == "pair_denied")
    })
    .await;
    assert!(r.store.lock().unwrap().hosts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_host_stops_reconnecting_until_retry() {
    let r = rig().await;
    r.mgr.connect(r.host());
    let mut g = GwConn::accept(&r.gw).await;
    match g.recv().await {
        PhoneMsg::Hello { pair_secret, .. } => assert_eq!(pair_secret, None),
        other => panic!("{other:?}"),
    }
    g.send(GatewayMsg::Rejected { reason: RejectReason::Revoked }).await;
    wait_for("rejected", || {
        matches!(r.mgr.state(), ConnState::Rejected { ref reason, .. } if reason == "revoked")
    })
    .await;
    // No automatic reconnect (the first backoff step would be 500 ms).
    let again = tokio::time::timeout(Duration::from_millis(1500), r.gw.accept()).await;
    assert!(again.is_err(), "must not reconnect after Rejected");
    // A manual retry connects again.
    r.mgr.retry_now();
    let mut g2 = GwConn::accept(&r.gw).await;
    assert!(matches!(g2.recv().await, PhoneMsg::Hello { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn incompatible_welcome_is_rejected() {
    let r = rig().await;
    r.mgr.connect(r.host());
    let mut g = GwConn::accept(&r.gw).await;
    g.recv().await;
    g.send(GatewayMsg::Welcome {
        proto_version: 9,
        min_compatible: 9,
        host_name: "x".into(),
        gateway_version: "9".into(),
        capabilities: vec![],
    })
    .await;
    wait_for("incompatible", || {
        matches!(r.mgr.state(), ConnState::Rejected { ref reason, .. } if reason == "incompatible")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnects_after_drop_and_resubscribes() {
    let r = rig().await;
    let sink = Arc::new(TestSink::default());
    r.mgr.connect(r.host());
    r.mgr.subscribe(term(1), 50, 20, true, sink.clone());

    let mut g = GwConn::accept(&r.gw).await;
    assert!(matches!(g.recv().await, PhoneMsg::Hello { .. }));
    g.welcome("My PC").await;
    match g.recv_msg().await {
        PhoneMsg::Subscribe { term: t, cols, rows, fit, replay_bytes } => {
            assert_eq!((t, cols, rows, fit), (term(1), 50, 20, true));
            assert_eq!(replay_bytes, REPLAY_BYTES);
        }
        other => panic!("{other:?}"),
    }
    g.send(GatewayMsg::Reset { term: term(1), cols: 50, rows: 20 }).await;
    g.send(GatewayMsg::Output { term: term(2), seq: 0, bytes: b"other".to_vec() }).await;
    g.send(GatewayMsg::Output { term: term(1), seq: 0, bytes: b"hello".to_vec() }).await;
    wait_for("output", || sink.events.lock().unwrap().len() >= 2).await;
    assert_eq!(*sink.events.lock().unwrap(), vec!["reset 50x20".to_string(), "out hello".to_string()]);

    // The phone resizes: the new size must be what gets re-subscribed.
    r.mgr.resize(term(1), 60, 30);
    assert!(matches!(g.recv_msg().await, PhoneMsg::Resize { cols: 60, rows: 30, .. }));

    // Connection drops.
    g.conn.close(7u32.into(), b"test");
    drop(g);
    wait_for("reconnecting", || matches!(r.mgr.state(), ConnState::Reconnecting { .. })).await;

    let mut g2 = GwConn::accept(&r.gw).await;
    assert!(matches!(g2.recv().await, PhoneMsg::Hello { pair_secret: None, .. }));
    g2.welcome("My PC").await;
    match g2.recv_msg().await {
        PhoneMsg::Subscribe { term: t, cols, rows, .. } => assert_eq!((t, cols, rows), (term(1), 60, 30)),
        other => panic!("{other:?}"),
    }
    wait_for("connected again", || matches!(r.mgr.state(), ConnState::Connected { .. })).await;
    let states = r.ui.states();
    assert!(states.iter().any(|s| matches!(s, ConnState::Reconnecting { attempt: 1, retry_in_ms: 500 })));
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_reach_the_gateway_and_upload_resolves() {
    let r = rig().await;
    r.mgr.connect(r.host());
    let mut g = GwConn::accept(&r.gw).await;
    g.recv().await;
    g.welcome("My PC").await;
    wait_for("connected", || matches!(r.mgr.state(), ConnState::Connected { .. })).await;

    r.mgr.input(term(3), vec![0x1b, b'[', b'Z']);
    r.mgr.send_text(term(3), "hi".into(), vec!["u1".into()], true);
    r.mgr.set_fit(term(3), false);
    r.mgr.mark_seen(term(3));
    assert_eq!(g.recv_msg().await, PhoneMsg::Input { term: term(3), bytes: vec![0x1b, b'[', b'Z'] });
    assert_eq!(
        g.recv_msg().await,
        PhoneMsg::SendText { term: term(3), text: "hi".into(), images: vec!["u1".into()], submit: true }
    );
    assert_eq!(g.recv_msg().await, PhoneMsg::SetFit { term: term(3), fit: false });
    assert_eq!(g.recv_msg().await, PhoneMsg::MarkSeen { term: term(3) });

    let mgr = r.mgr.clone();
    let up = tokio::spawn(async move { mgr.upload("a.jpg".into(), vec![1, 2, 3]).await });
    let req_id = match g.recv_msg().await {
        PhoneMsg::UploadImage { req_id, file_name, data } => {
            assert_eq!((file_name.as_str(), data.as_slice()), ("a.jpg", &[1u8, 2, 3][..]));
            req_id
        }
        other => panic!("{other:?}"),
    };
    g.send(GatewayMsg::Uploaded { req_id, result: Ok("up-1".into()) }).await;
    assert_eq!(up.await.unwrap(), Ok("up-1".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_while_disconnected_fails_fast() {
    let r = rig().await;
    assert_eq!(r.mgr.upload("a.jpg".into(), vec![1]).await, Err("not connected".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_upload_is_refused_locally() {
    let r = rig().await;
    let big = vec![0u8; MAX_UPLOAD_BYTES + 1];
    assert!(r.mgr.upload("big.png".into(), big).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_returns_to_idle_and_closes() {
    let r = rig().await;
    r.mgr.connect(r.host());
    let mut g = GwConn::accept(&r.gw).await;
    g.recv().await;
    g.welcome("My PC").await;
    wait_for("connected", || matches!(r.mgr.state(), ConnState::Connected { .. })).await;
    r.mgr.stop();
    wait_for("idle", || r.mgr.state() == ConnState::Idle).await;
    // The gateway sees the stream/connection end.
    let end = tokio::time::timeout(Duration::from_secs(5), g.recv.next()).await;
    assert!(matches!(end, Ok(None) | Ok(Some(Err(_)))));
}

#[derive(Default)]
struct TestChatSink {
    events: Mutex<Vec<serde_json::Value>>,
}

impl crate::chat::ChatSink for TestChatSink {
    fn event(&self, ev: serde_json::Value) {
        self.events.lock().unwrap().push(ev);
    }
}

fn chat_msg(id: &str, order: u64) -> sich_remote_proto::ChatMessage {
    sich_remote_proto::ChatMessage {
        id: id.into(),
        order,
        role: sich_remote_proto::ChatRole::User,
        timestamp_ms: None,
        blocks: vec![sich_remote_proto::ChatBlock::Text { markdown: "hello".into() }],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_events_reach_the_sink_and_resubscribe_after_reconnect() {
    use sich_remote_proto::ChatStatus;
    let r = rig().await;
    let sink = Arc::new(TestChatSink::default());
    r.mgr.connect(r.host());
    r.mgr.chat_subscribe(term(1), 50, sink.clone());

    let mut g = GwConn::accept(&r.gw).await;
    assert!(matches!(g.recv().await, PhoneMsg::Hello { .. }));
    g.send(GatewayMsg::Welcome {
        proto_version: REMOTE_PROTO_VERSION,
        min_compatible: MIN_COMPATIBLE,
        host_name: "My PC".into(),
        gateway_version: "test".into(),
        capabilities: vec!["chat".into()],
    })
    .await;
    assert_eq!(g.recv_msg().await, PhoneMsg::ChatSubscribe { term: term(1), limit: 50 });
    wait_for("connected", || matches!(r.mgr.state(), ConnState::Connected { .. })).await;
    assert_eq!(r.mgr.capabilities(), vec!["chat".to_string()]);

    g.send(GatewayMsg::ChatSnapshot {
        term: term(1),
        status: ChatStatus::Ready,
        messages: vec![chat_msg("a", 0)],
        has_more: true,
    })
    .await;
    // Other terminals' chat events are ignored.
    g.send(GatewayMsg::ChatUpdate { term: term(2), messages: vec![chat_msg("x", 1)] }).await;
    g.send(GatewayMsg::ChatUpdate { term: term(1), messages: vec![chat_msg("b", 10)] }).await;
    g.send(GatewayMsg::ChatEarlier { term: term(1), messages: vec![], has_more: false }).await;
    wait_for("chat events", || sink.events.lock().unwrap().len() >= 3).await;
    {
        let ev = sink.events.lock().unwrap();
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[0]["kind"], "snapshot");
        assert_eq!(ev[0]["status"]["state"], "ready");
        assert_eq!(ev[0]["has_more"], true);
        assert_eq!(ev[1]["kind"], "update");
        assert_eq!(ev[1]["messages"][0]["id"], "b");
        assert_eq!(ev[2]["kind"], "earlier");
    }

    r.mgr.chat_load_earlier(term(1), 0, 30);
    assert_eq!(g.recv_msg().await, PhoneMsg::ChatLoadEarlier { term: term(1), before_order: 0, limit: 30 });

    // Reconnect: the chat is re-subscribed.
    g.conn.close(7u32.into(), b"test");
    drop(g);
    let mut g2 = GwConn::accept(&r.gw).await;
    assert!(matches!(g2.recv().await, PhoneMsg::Hello { .. }));
    g2.welcome("My PC").await;
    assert_eq!(g2.recv_msg().await, PhoneMsg::ChatSubscribe { term: term(1), limit: 50 });

    // Unsubscribe: no more events, and the gateway is told.
    r.mgr.chat_unsubscribe(term(1));
    assert_eq!(g2.recv_msg().await, PhoneMsg::ChatUnsubscribe { term: term(1) });
    g2.send(GatewayMsg::ChatUpdate { term: term(1), messages: vec![chat_msg("c", 20)] }).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(sink.events.lock().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_image_resolves_by_req_id_and_fails_offline() {
    let r = rig().await;
    // Not connected: fails fast.
    assert_eq!(r.mgr.chat_image(term(1), "ref".into()).await.unwrap_err(), "not connected");

    r.mgr.connect(r.host());
    let mut g = GwConn::accept(&r.gw).await;
    g.recv().await;
    g.welcome("My PC").await;
    wait_for("connected", || matches!(r.mgr.state(), ConnState::Connected { .. })).await;
    assert!(r.mgr.capabilities().is_empty());

    let mgr = r.mgr.clone();
    let ok = tokio::spawn(async move { mgr.chat_image(term(1), "img-1".into()).await });
    let req_id = match g.recv_msg().await {
        PhoneMsg::ChatImage { req_id, term: t, image_ref } => {
            assert_eq!((t, image_ref.as_str()), (term(1), "img-1"));
            req_id
        }
        other => panic!("{other:?}"),
    };
    g.send(GatewayMsg::ChatImageData {
        req_id,
        result: Ok(sich_remote_proto::ChatImageBytes { mime: "image/png".into(), data: vec![1, 2] }),
    })
    .await;
    let got = ok.await.unwrap().unwrap();
    assert_eq!((got.mime.as_str(), got.data.as_slice()), ("image/png", &[1u8, 2][..]));

    let mgr = r.mgr.clone();
    let bad = tokio::spawn(async move { mgr.chat_image(term(1), "img-2".into()).await });
    let req_id = match g.recv_msg().await {
        PhoneMsg::ChatImage { req_id, .. } => req_id,
        other => panic!("{other:?}"),
    };
    g.send(GatewayMsg::ChatImageData { req_id, result: Err("gone".into()) }).await;
    assert_eq!(bad.await.unwrap().unwrap_err(), "gone");
}
