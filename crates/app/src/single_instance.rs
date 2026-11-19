//! The UI socket supports foregrounding the current GUI or handing it over to a newer launch.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use interprocess::local_socket::tokio::{Listener, Stream, prelude::*};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name,
};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::time::{Instant, sleep, timeout};

use crate::tray::UiEvent;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(750);
const TAKEOVER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Line<'a> {
    Show,
    Hello(&'a str),
    Build(&'a str),
    Takeover(&'a str),
}

impl<'a> Line<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let line = line.strip_suffix('\n')?.trim();
        if line == "show" {
            return Some(Self::Show);
        }
        let (command, id) = line.split_once(' ')?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')) {
            return None;
        }
        match command {
            "hello" => Some(Self::Hello(id)),
            "build" => Some(Self::Build(id)),
            "takeover" => Some(Self::Takeover(id)),
            _ => None,
        }
    }

    fn format(self) -> String {
        match self {
            Self::Show => "show\n".into(),
            Self::Hello(id) => format!("hello {id}\n"),
            Self::Build(id) => format!("build {id}\n"),
            Self::Takeover(id) => format!("takeover {id}\n"),
        }
    }
}

fn decide<'a>(new: &'a str, existing: &str, force: bool) -> Line<'a> {
    if force || new != existing { Line::Takeover(new) } else { Line::Show }
}

fn ui_socket_name(daemon_socket: &str) -> String {
    format!("{daemon_socket}-ui")
}

fn to_name(name: &str) -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        name.to_string().to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir().join(format!("{name}.sock")).to_fs_name::<GenericFilePath>()
    }
}

pub enum Acquire {
    Primary(Listener),
    AlreadyRunning,
    Unavailable,
}

async fn read_line(stream: &mut BufReader<Stream>) -> io::Result<String> {
    let mut line = String::new();
    timeout(HANDSHAKE_TIMEOUT, stream.take(256).read_line(&mut line)).await??;
    Ok(line)
}

async fn negotiate(stream: Stream, build: &str, force: bool) -> io::Result<bool> {
    let mut stream = BufReader::new(stream);
    stream.write_all(Line::Hello(build).format().as_bytes()).await?;
    let line = read_line(&mut stream).await?;
    let Some(Line::Build(existing)) = Line::parse(&line) else {
        return Err(io::ErrorKind::InvalidData.into());
    };
    let command = decide(build, existing, force);
    let takeover = matches!(command, Line::Takeover(_));
    stream.write_all(command.format().as_bytes()).await?;
    Ok(takeover)
}

pub async fn acquire(daemon_socket: &str, build: &str, force: bool) -> Acquire {
    let socket = ui_socket_name(daemon_socket);
    let name = match to_name(&socket) {
        Ok(name) => name,
        Err(err) => {
            tracing::warn!("single instance: invalid socket name: {err}");
            return Acquire::Unavailable;
        }
    };
    let mut takeover_requested = false;
    let mut deadline = Instant::now() + TAKEOVER_TIMEOUT;
    loop {
        if let Ok(Ok(stream)) = timeout(HANDSHAKE_TIMEOUT, Stream::connect(name.borrow())).await {
            if !takeover_requested {
                #[cfg(windows)]
                unsafe {
                    // The running instance may need to foreground its window.
                    windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX);
                }
                match timeout(HANDSHAKE_TIMEOUT, negotiate(stream, build, force)).await {
                    Ok(Ok(true)) => {
                        takeover_requested = true;
                        deadline = Instant::now() + TAKEOVER_TIMEOUT;
                    }
                    Ok(Ok(false)) => return Acquire::AlreadyRunning,
                    _ => {
                        // Older UIs only understand "show".
                        if let Ok(Ok(mut legacy)) = timeout(HANDSHAKE_TIMEOUT, Stream::connect(name.borrow())).await {
                            let _ = timeout(HANDSHAKE_TIMEOUT, legacy.write_all(Line::Show.format().as_bytes())).await;
                        }
                        return Acquire::AlreadyRunning;
                    }
                }
            }
        } else {
            match ListenerOptions::new().name(name.borrow()).create_tokio() {
                Ok(listener) => return Acquire::Primary(listener),
                Err(err) if err.kind() == io::ErrorKind::AddrInUse => {},
                Err(err) if !takeover_requested => {
                    tracing::warn!("single instance: cannot listen: {err}");
                    return Acquire::Unavailable;
                }
                Err(_) => {},
            }
        }
        if Instant::now() >= deadline {
            tracing::warn!("single instance: UI socket did not become available");
            return Acquire::AlreadyRunning;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

pub async fn serve(listener: Listener, build: &'static str, on_event: impl Fn(UiEvent) + Send + Sync + 'static) {
    let on_event = Arc::new(on_event);
    while let Ok(stream) = listener.accept().await {
        let on_event = on_event.clone();
        tokio::spawn(async move {
            if let Ok(Some(event)) = receive(stream, build).await {
                on_event(event);
            }
        });
    }
}

async fn receive(stream: Stream, build: &str) -> io::Result<Option<UiEvent>> {
    let mut stream = BufReader::new(stream);
    let line = read_line(&mut stream).await?;
    if Line::parse(&line) == Some(Line::Show) {
        return Ok(Some(UiEvent::Show));
    }
    let Some(Line::Hello(peer)) = Line::parse(&line) else { return Ok(None) };
    stream.write_all(Line::Build(build).format().as_bytes()).await?;
    let next = read_line(&mut stream).await?;
    Ok(match Line::parse(&next) {
        Some(Line::Show) => Some(UiEvent::Show),
        Some(Line::Takeover(id)) if id == peer => Some(UiEvent::Takeover),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique_socket() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!("sich-ui-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn ui_socket_is_derived_from_the_daemon_socket() {
        assert_eq!(ui_socket_name("sich-daemon"), "sich-daemon-ui");
        assert_ne!(ui_socket_name("a"), ui_socket_name("b"));
    }

    #[test]
    fn line_protocol_round_trips_and_rejects_malformed_lines() {
        for command in [Line::Show, Line::Hello("0.0.1-ab123-123"), Line::Build("b"), Line::Takeover("b")] {
            let line = command.format();
            assert_eq!(Line::parse(&line), Some(command));
        }
        assert_eq!(Line::parse("show\r\n"), Some(Line::Show));
        for bad in ["show", "quit\n", "hello\n", "build \n", "takeover ab cd\n", "hello abc/def\n"] {
            assert_eq!(Line::parse(bad), None);
        }
    }

    #[test]
    fn decision_uses_build_and_explicit_restart() {
        assert_eq!(decide("new", "new", false), Line::Show);
        assert_eq!(decide("new", "old", false), Line::Takeover("new"));
        assert_eq!(decide("new", "new", true), Line::Takeover("new"));
    }

    #[tokio::test]
    async fn same_build_shows_and_different_build_waits_for_handover() {
        let sock = unique_socket();
        let Acquire::Primary(listener) = acquire(&sock, "old", false).await else { panic!("primary") };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve(listener, "old", move |event| { let _ = tx.send(event); }));
        assert!(matches!(acquire(&sock, "old", false).await, Acquire::AlreadyRunning));
        assert_eq!(timeout(HANDSHAKE_TIMEOUT, rx.recv()).await.unwrap(), Some(UiEvent::Show));

        let next = tokio::spawn(async move { acquire(&sock, "new", false).await });
        assert_eq!(timeout(HANDSHAKE_TIMEOUT, rx.recv()).await.unwrap(), Some(UiEvent::Takeover));
        assert!(!next.is_finished());
        server.abort();
        let _ = server.await;
        assert!(matches!(next.await.unwrap(), Acquire::Primary(_)));
    }

    #[tokio::test]
    async fn existing_server_accepts_plain_show_and_ignores_mismatched_takeover() {
        let sock = unique_socket();
        let Acquire::Primary(listener) = acquire(&sock, "old", false).await else { panic!("primary") };
        assert!(matches!(acquire(&unique_socket(), "old", false).await, Acquire::Primary(_)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _server = tokio::spawn(serve(listener, "old", move |event| { let _ = tx.send(event); }));
        let name = to_name(&ui_socket_name(&sock)).unwrap();
        let mut wrong = BufReader::new(Stream::connect(name.borrow()).await.unwrap());
        wrong.write_all(b"hello one\n").await.unwrap();
        assert_eq!(read_line(&mut wrong).await.unwrap(), "build old\n");
        wrong.write_all(b"takeover another\n").await.unwrap();
        assert_eq!(read_line(&mut wrong).await.unwrap(), "");
        let mut plain = Stream::connect(name).await.unwrap();
        plain.write_all(b"show\n").await.unwrap();
        assert_eq!(timeout(HANDSHAKE_TIMEOUT, rx.recv()).await.unwrap(), Some(UiEvent::Show));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_legacy_show_only_instance_is_foregrounded() {
        let sock = unique_socket();
        let Acquire::Primary(listener) = acquire(&sock, "old", false).await else { panic!("primary") };
        let legacy = tokio::spawn(async move {
            let hello = listener.accept().await.unwrap();
            assert_eq!(read_line(&mut BufReader::new(hello)).await.unwrap(), "hello new\n");
            let show = listener.accept().await.unwrap();
            assert_eq!(read_line(&mut BufReader::new(show)).await.unwrap(), "show\n");
        });
        assert!(matches!(acquire(&sock, "new", false).await, Acquire::AlreadyRunning));
        legacy.await.unwrap();
    }
}
