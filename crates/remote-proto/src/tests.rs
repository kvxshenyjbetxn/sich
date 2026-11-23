// Framed-codec tests need `--features tokio` (or `--all-features`).
use crate::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;

fn rt<T: Serialize + DeserializeOwned + PartialEq + Debug>(v: T) {
    let body = encode(&v);
    assert_eq!(decode::<T>(&body).unwrap(), v);
    let fr = frame(&v);
    assert_eq!(u32::from_be_bytes(fr[..4].try_into().unwrap()) as usize, fr.len() - 4);
    assert_eq!(&fr[4..], &body[..]);
}

fn t(id: u64) -> TermRef {
    TermRef { daemon: "sich-abc".into(), id }
}

fn agent() -> Agent {
    Agent {
        kind: AgentKind::Claude,
        state: AgentState::Waiting,
        unread: true,
        since_unix_ms: 1_700_000_000_000,
        title: Some("t".into()),
        detail: None,
        model: Some("opus".into()),
        prompt: Some("fix".into()),
        message: Some("ok".into()),
        subagents_working: 2,
    }
}

fn workspace() -> Workspace {
    Workspace {
        host_name: "PC".into(),
        groups: vec![Group { id: 1, name: "G".into() }],
        projects: vec![Project {
            id: 7,
            name: "sich".into(),
            path: r"E:\dev\sich".into(),
            is_git: true,
            group: Some(1),
            worktrees: vec![
                Worktree { path: r"E:\dev\sich".into(), branch: Some("master".into()), is_main: true, dirty: false },
                Worktree { path: r"E:\wt\x".into(), branch: None, is_main: false, dirty: true },
            ],
            error: Some("boom".into()),
        }],
        terminals: vec![
            Terminal {
                term: t(3),
                title: "claude".into(),
                cwd: r"E:\dev\sich".into(),
                worktree: Some(r"E:\wt\x".into()),
                cols: 120,
                rows: 30,
                exited: Some(Some(1)),
                created_unix_ms: 5,
                agent: Some(agent()),
            },
            Terminal {
                term: t(4),
                title: "sh".into(),
                cwd: "/".into(),
                worktree: None,
                cols: 80,
                rows: 24,
                exited: Some(None),
                created_unix_ms: 6,
                agent: None,
            },
            Terminal {
                term: t(5),
                title: "".into(),
                cwd: "".into(),
                worktree: None,
                cols: 1,
                rows: 1,
                exited: None,
                created_unix_ms: 0,
                agent: None,
            },
        ],
    }
}

#[test]
fn phone_msgs_round_trip() {
    let msgs = vec![
        PhoneMsg::Hello {
            proto_version: 1,
            min_compatible: 1,
            app_version: "0.1".into(),
            device_name: "Pixel".into(),
            capabilities: vec!["a".into(), "b".into()],
            pair_secret: Some("s".into()),
        },
        PhoneMsg::Hello {
            proto_version: 1,
            min_compatible: 1,
            app_version: "".into(),
            device_name: "".into(),
            capabilities: vec![],
            pair_secret: None,
        },
        PhoneMsg::Ping { nonce: u64::MAX },
        PhoneMsg::Subscribe { term: t(1), cols: 80, rows: 24, fit: true, replay_bytes: 65536 },
        PhoneMsg::Unsubscribe { term: t(1) },
        PhoneMsg::Input { term: t(1), bytes: vec![0, 27, 255] },
        PhoneMsg::SendText { term: t(1), text: "привіт\nworld".into(), images: vec!["u1".into()], submit: true },
        PhoneMsg::Resize { term: t(1), cols: 100, rows: 40 },
        PhoneMsg::SetFit { term: t(1), fit: false },
        PhoneMsg::MarkSeen { term: t(1) },
        PhoneMsg::UploadImage { req_id: 9, file_name: "a.png".into(), data: vec![1; 1000] },
    ];
    for m in msgs {
        rt(m);
    }
}

#[test]
fn gateway_msgs_round_trip() {
    let mut msgs = vec![
        GatewayMsg::Welcome {
            proto_version: 1,
            min_compatible: 1,
            host_name: "PC".into(),
            gateway_version: "0.1.38".into(),
            capabilities: vec!["x".into()],
        },
        GatewayMsg::PairPending,
        GatewayMsg::Pong { nonce: 5 },
        GatewayMsg::Workspace(workspace()),
        GatewayMsg::Workspace(Workspace { host_name: "".into(), groups: vec![], projects: vec![], terminals: vec![] }),
        GatewayMsg::Reset { term: t(2), cols: 80, rows: 24 },
        GatewayMsg::Output { term: t(2), seq: 1 << 40, bytes: b"\x1b[0mhi".to_vec() },
        GatewayMsg::TermSize { term: t(2), cols: 80, rows: 24, fit: true },
        GatewayMsg::Uploaded { req_id: 1, result: Ok("id1".into()) },
        GatewayMsg::Uploaded { req_id: 2, result: Err("too big".into()) },
        GatewayMsg::Exited { term: t(2), code: Some(-1) },
        GatewayMsg::Exited { term: t(2), code: None },
        GatewayMsg::Error { message: "oops".into() },
    ];
    for reason in [
        RejectReason::Unpaired,
        RejectReason::PairSecretInvalid,
        RejectReason::PairDenied,
        RejectReason::PairTimeout,
        RejectReason::Revoked,
        RejectReason::Incompatible { gateway_version: 3, gateway_min: 2 },
    ] {
        msgs.push(GatewayMsg::Rejected { reason });
    }
    for m in msgs {
        rt(m);
    }
}

#[test]
fn enums_round_trip() {
    for k in [AgentKind::Claude, AgentKind::Gemini, AgentKind::Codex, AgentKind::Omp, AgentKind::Other] {
        rt(k);
    }
    for s in [AgentState::Working, AgentState::Waiting, AgentState::Done, AgentState::Failed, AgentState::Idle] {
        rt(s);
    }
}

#[test]
fn hello_and_welcome_are_variant_zero() {
    let hello = PhoneMsg::Hello {
        proto_version: 1,
        min_compatible: 1,
        app_version: "".into(),
        device_name: "".into(),
        capabilities: vec![],
        pair_secret: None,
    };
    assert_eq!(encode(&hello)[0], 0);
    let welcome = GatewayMsg::Welcome {
        proto_version: 1,
        min_compatible: 1,
        host_name: "".into(),
        gateway_version: "".into(),
        capabilities: vec![],
    };
    assert_eq!(encode(&welcome)[0], 0);
    // Variant order is wire format.
    assert_eq!(encode(&PhoneMsg::Ping { nonce: 0 })[0], 1);
    assert_eq!(encode(&PhoneMsg::MarkSeen { term: t(0) })[0], 8);
    assert_eq!(encode(&GatewayMsg::PairPending)[0], 2);
    assert_eq!(encode(&GatewayMsg::Error { message: String::new() })[0], 10);
}

#[test]
fn decode_garbage_errors() {
    assert!(decode::<PhoneMsg>(&[]).is_err());
    assert!(decode::<PhoneMsg>(&[200]).is_err());
    assert!(decode::<GatewayMsg>(&[1]).is_err());
}

#[test]
fn termref_ordering_and_hash() {
    use std::collections::HashSet;
    assert!(t(1) < t(2));
    let mut a = t(1);
    a.daemon = "a".into();
    assert!(a < t(1));
    let set: HashSet<_> = [t(1), t(1), t(2)].into_iter().collect();
    assert_eq!(set.len(), 2);
}

#[test]
fn constants() {
    assert_eq!(ALPN, b"sich/remote/1");
    assert_eq!(REMOTE_PROTO_VERSION, 1);
    assert_eq!(MIN_COMPATIBLE, 1);
    assert_eq!(MAX_FRAME, 16 * 1024 * 1024);
}

#[test]
fn compatible_table() {
    // (their_version, their_min, expected) with mine = (1, 1)
    for (v, m, ok) in [(1, 1, true), (2, 1, true), (5, 1, true), (2, 2, false), (3, 2, false), (0, 0, false), (1, 0, true)] {
        assert_eq!(compatible(v, m), ok, "v={v} min={m}");
    }
}

#[cfg(feature = "tokio")]
mod framed {
    use super::*;
    use bytes::BytesMut;
    use tokio_util::codec::{Decoder, Encoder};

    #[test]
    fn framed_round_trip() {
        let mut c = frame_codec();
        let msg = GatewayMsg::Workspace(workspace());
        let mut buf = BytesMut::new();
        c.encode(encode_frame(&msg).unwrap(), &mut buf).unwrap();
        // Same bytes as the plain `frame` helper.
        assert_eq!(&buf[..], &frame(&msg)[..]);
        let body = c.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decode_frame::<GatewayMsg>(&body).unwrap(), msg);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_frame_waits() {
        let mut c = frame_codec();
        let full = frame(&PhoneMsg::Ping { nonce: 1 });
        let mut buf = BytesMut::from(&full[..full.len() - 1]);
        assert!(c.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&full[full.len() - 1..]);
        let body = c.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decode_frame::<PhoneMsg>(&body).unwrap(), PhoneMsg::Ping { nonce: 1 });
    }

    #[test]
    fn oversized_frame_rejected() {
        // Incoming: header claims more than MAX_FRAME.
        let mut c = frame_codec();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        buf.extend_from_slice(&[0; 16]);
        assert!(c.decode(&mut buf).is_err());
        // Outgoing: body larger than MAX_FRAME.
        let big = PhoneMsg::UploadImage { req_id: 1, file_name: "x".into(), data: vec![0; MAX_FRAME + 1] };
        assert!(matches!(encode_frame(&big), Err(Error::TooLarge(_))));
        let mut c = frame_codec();
        let mut out = BytesMut::new();
        assert!(c.encode(bytes::Bytes::from(vec![0u8; MAX_FRAME + 1]), &mut out).is_err());
    }
}

mod pair {
    use super::*;

    fn sample() -> PairUrl {
        PairUrl {
            endpoint_id: "abcdef0123".into(),
            addrs: vec!["192.168.1.5:1234".into(), "[fe80::1]:99".into()],
            relay: Some("https://relay.example.com/a?b=c&d=e".into()),
            secret: "AbC-_123".into(),
            host: "Мій ПК & co = \"x\" 100%".into(),
        }
    }

    #[test]
    fn round_trip() {
        let p = sample();
        let url = p.to_url();
        assert!(url.starts_with("sich://pair?v=1&id=abcdef0123&addrs="));
        assert!(!url.contains(' ') && !url.contains('#'));
        assert_eq!(PairUrl::parse(&url).unwrap(), p);
    }

    #[test]
    fn empty_addrs_and_relay() {
        let p = PairUrl { addrs: vec![], relay: None, ..sample() };
        let url = p.to_url();
        assert!(url.contains("&addrs=&relay=&"));
        assert_eq!(PairUrl::parse(&url).unwrap(), p);
    }

    #[test]
    fn empty_host_ok() {
        let p = PairUrl { host: String::new(), ..sample() };
        assert_eq!(PairUrl::parse(&p.to_url()).unwrap(), p);
    }

    #[test]
    fn unknown_params_ignored_and_order_free() {
        let p = PairUrl::parse("sich://pair?x=1&secret=s&id=i&v=1&zzz&host=h%20x&addrs=a%3A1,b%3A2&relay=").unwrap();
        assert_eq!(p.endpoint_id, "i");
        assert_eq!(p.secret, "s");
        assert_eq!(p.host, "h x");
        assert_eq!(p.addrs, vec!["a:1", "b:2"]);
        assert_eq!(p.relay, None);
    }

    #[test]
    fn rejects() {
        use PairUrlError::*;
        let ok = "id=i&secret=s";
        assert_eq!(PairUrl::parse(&format!("http://pair?v=1&{ok}")), Err(BadScheme));
        assert_eq!(PairUrl::parse(&format!("sich://other?v=1&{ok}")), Err(BadScheme));
        assert_eq!(PairUrl::parse(""), Err(BadScheme));
        assert_eq!(PairUrl::parse(&format!("sich://pairx?v=1&{ok}")), Err(BadScheme));
        assert_eq!(PairUrl::parse(&format!("sich://pair?v=2&{ok}")), Err(BadVersion));
        assert_eq!(PairUrl::parse(&format!("sich://pair?{ok}")), Err(BadVersion));
        assert_eq!(PairUrl::parse("sich://pair?v=1&secret=s"), Err(Missing("id")));
        assert_eq!(PairUrl::parse("sich://pair?v=1&id=&secret=s"), Err(Missing("id")));
        assert_eq!(PairUrl::parse("sich://pair?v=1&id=i"), Err(Missing("secret")));
        assert_eq!(PairUrl::parse("sich://pair?v=1&id=i&secret="), Err(Missing("secret")));
        assert_eq!(PairUrl::parse("sich://pair?v=1&id=%zz&secret=s"), Err(BadEncoding));
        assert_eq!(PairUrl::parse("sich://pair?v=1&id=%4&secret=s"), Err(BadEncoding));
        assert_eq!(PairUrl::parse("sich://pair?v=1&id=%FF&secret=s"), Err(BadEncoding));
    }
}
