//! Single GUI instance: a second `sich` launch asks the running one to show its window and
//! exits. Uses a local socket named `<daemon socket>-ui`, so isolated instances (a different
//! `SICH_SOCKET`) do not collide.

use std::io::{self, BufRead as _, BufReader, Write as _};
use std::thread::JoinHandle;

use interprocess::local_socket::traits::{ListenerExt as _, Stream as _};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, Listener, ListenerOptions, Name, NameType as _, Stream,
    ToFsName as _,
    ToNsName as _,
};

/// The line sent by a second instance.
const SHOW: &str = "show";

/// Name of the UI socket for the given daemon socket name.
pub fn ui_socket_name(daemon_socket: &str) -> String {
    format!("{daemon_socket}-ui")
}

fn to_name(name: &str) -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        name.to_string().to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir().join(format!("{name}.sock")).to_fs_name::<GenericFilePath>()
    }
}

/// Result of [`acquire`].
pub enum Acquire {
    /// We are the first instance; keep the listener and call [`serve`].
    Primary(Listener),
    /// Another instance is running and was asked to show its window.
    AlreadyRunning,
    /// The socket could not be used; carry on without single-instance protection.
    Unavailable,
}

/// Tells a running instance to show its window.
fn signal_existing(name: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        // Let the instance we are about to wake bring its window to the foreground.
        // SAFETY: plain Win32 call without pointers.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX);
        }
    }
    let mut stream = Stream::connect(to_name(name)?)?;
    stream.write_all(format!("{SHOW}\n").as_bytes())?;
    stream.flush()
}

/// Signals a running instance, or becomes the primary one.
pub fn acquire(daemon_socket: &str) -> Acquire {
    let name = ui_socket_name(daemon_socket);
    if signal_existing(&name).is_ok() {
        return Acquire::AlreadyRunning;
    }
    let name_obj = match to_name(&name) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("single instance: bad socket name: {e}");
            return Acquire::Unavailable;
        }
    };
    match ListenerOptions::new().name(name_obj).create_sync() {
        Ok(l) => Acquire::Primary(l),
        // Lost a race with another instance that started at the same moment.
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if signal_existing(&name).is_ok() { Acquire::AlreadyRunning } else { Acquire::Unavailable }
        }
        Err(e) => {
            tracing::warn!("single instance: cannot listen: {e}");
            Acquire::Unavailable
        }
    }
}

/// Runs the accept loop on a background thread; `on_show` is called for every "show" request.
pub fn serve(listener: Listener, on_show: impl Fn() + Send + Sync + 'static) -> JoinHandle<()> {
    let on_show = std::sync::Arc::new(on_show);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let on_show = on_show.clone();
            // One short-lived thread per connection: a silent client must not block the loop.
            std::thread::spawn(move || {
                if read_show(conn) {
                    on_show();
                }
            });
        }
    })
}

/// Reads one request line (at most 64 bytes); true when it is "show".
fn read_show(conn: impl io::Read) -> bool {
    let mut line = String::new();
    let mut reader = BufReader::new(conn.take(64));
    matches!(reader.read_line(&mut line), Ok(n) if n > 0) && is_show(&line)
}

/// True for the "show" request line.
pub fn is_show(line: &str) -> bool {
    line.trim() == SHOW
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn unique_socket() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        format!("sich-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
    }

    #[test]
    fn ui_socket_is_derived_from_the_daemon_socket() {
        assert_eq!(ui_socket_name("sich-daemon"), "sich-daemon-ui");
        assert_ne!(ui_socket_name("a"), ui_socket_name("b"));
    }

    #[test]
    fn only_show_lines_are_accepted() {
        assert!(is_show("show\n"));
        assert!(is_show("show\r\n"));
        assert!(!is_show("quit\n"));
        assert!(!is_show(""));
    }

    #[test]
    fn second_instance_delivers_show_to_the_first() {
        let sock = unique_socket();
        let Acquire::Primary(listener) = acquire(&sock) else { panic!("first instance must be primary") };
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let _t = serve(listener, move || {
            let _ = tx.lock().unwrap().send(());
        });

        assert!(matches!(acquire(&sock), Acquire::AlreadyRunning));
        rx.recv_timeout(Duration::from_secs(5)).expect("show request delivered");
        // And again: the listener keeps serving.
        assert!(matches!(acquire(&sock), Acquire::AlreadyRunning));
        rx.recv_timeout(Duration::from_secs(5)).expect("second show request delivered");
    }

    #[test]
    fn other_requests_do_not_trigger_show() {
        let sock = unique_socket();
        let Acquire::Primary(listener) = acquire(&sock) else { panic!("primary") };
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let _t = serve(listener, move || {
            let _ = tx.lock().unwrap().send(());
        });
        let mut s = Stream::connect(to_name(&ui_socket_name(&sock)).unwrap()).unwrap();
        s.write_all(b"hello\n").unwrap();
        drop(s);
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn different_sockets_do_not_collide() {
        let (a, b) = (unique_socket(), unique_socket());
        let Acquire::Primary(_la) = acquire(&a) else { panic!("a primary") };
        assert!(matches!(acquire(&b), Acquire::Primary(_)));
    }
}
