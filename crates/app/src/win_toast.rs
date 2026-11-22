//! Native toasts. The worker owns WinRT objects and handles registration and history calls.

use futures::channel::mpsc::UnboundedSender;
use sich_proto::TermId;

use crate::tray::UiEvent;

#[cfg(not(windows))]
pub fn show(_xml: &str, _term_id: Option<TermId>, _on_click: UnboundedSender<UiEvent>) {}

#[cfg(not(windows))]
pub fn remove(_term_id: TermId) {}

#[cfg(windows)]
pub use imp::{remove, show};

#[cfg(windows)]
mod imp {
    use std::collections::VecDeque;
    use std::ffi::{OsStr, c_void};
    use std::io::{self, Write as _};
    use std::os::windows::ffi::OsStrExt as _;
    use std::sync::{OnceLock, mpsc};

    use anyhow::Context as _;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastNotifier};
    use windows::Win32::Foundation::CLASS_E_NOAGGREGATION;
    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, CoCreateGuid, CoRegisterClassObject, CoRevokeClassObject,
        IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
    };
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
    use windows::Win32::UI::Notifications::{
        INotificationActivationCallback, INotificationActivationCallback_Impl,
        NOTIFICATION_USER_INPUT_DATA,
    };
    use windows::core::{BOOL, GUID, HSTRING, IUnknown, Interface as _, PCWSTR, Ref, implement, w};
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};

    use super::*;

    const AUMID: &str = "Sich.Sich";
    const GROUP: &str = "agent";
    const TEST_TAG: &str = "test";
    const ACTIVATOR: GUID = GUID::from_u128(0x6bef5624_ebb3_4553_ac90_a569c14e304e);
    const MAX_RECENT_TOASTS: usize = 32;
    static WORKER: OnceLock<Option<mpsc::Sender<Command>>> = OnceLock::new();

    enum Command {
        Show(String, Option<TermId>, UnboundedSender<UiEvent>),
        Remove(TermId),
        Activate(String),
    }

    pub fn show(xml: &str, term_id: Option<TermId>, on_click: UnboundedSender<UiEvent>) {
        send(Command::Show(xml.to_owned(), term_id, on_click));
    }

    pub fn remove(term_id: TermId) {
        send(Command::Remove(term_id));
    }

    fn send(command: Command) {
        let worker = WORKER.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            let activation_tx = tx.clone();
            match std::thread::Builder::new()
                .name("sich-toasts".into())
                .spawn(move || {
                    if let Err(err) = run(rx, activation_tx) {
                        tracing::warn!(%err, "Windows toast worker stopped");
                    }
                }) {
                Ok(_) => Some(tx),
                Err(err) => {
                    tracing::warn!(%err, "could not start the Windows toast worker");
                    None
                }
            }
        });
        if let Some(tx) = worker {
            let _ = tx.send(command);
        }
    }

    fn run(rx: mpsc::Receiver<Command>, tx: mpsc::Sender<Command>) -> anyhow::Result<()> {
        // This dedicated thread has no other COM apartment or message loop.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        let mut notifier = None;
        let mut activator = None;
        let mut recent: VecDeque<LiveToast> = VecDeque::new();
        for command in rx {
            let result = match command {
                Command::Show(xml, term_id, on_click) => show_toast(
                    &mut notifier,
                    &mut activator,
                    &mut recent,
                    &tx,
                    &xml,
                    term_id,
                    on_click,
                ),
                Command::Remove(id) => {
                    let tag = id.to_string();
                    recent.retain(|entry| entry.tag != tag);
                    remove_toast(&tag).context("remove toast from Windows history")
                }
                Command::Activate(launch) => {
                    if let Some(entry) = recent.iter_mut().find(|entry| entry.launch == launch)
                        && let Some(tx) = entry.on_click.take()
                    {
                        let _ = tx.unbounded_send(entry.event);
                    }
                    Ok(())
                }
            };
            if let Err(err) = result {
                tracing::warn!(error = %format_args!("{err:#}"), "Windows toast operation failed");
            }
        }
        drop(recent);
        drop(notifier);
        if let Some(cookie) = activator
            && let Err(err) = unsafe { CoRevokeClassObject(cookie) }
        {
            tracing::warn!(%err, "could not revoke the Windows toast activator");
        }
        unsafe { RoUninitialize() };
        Ok(())
    }

    struct LiveToast {
        toast: ToastNotification,
        token: i64,
        tag: String,
        launch: String,
        event: UiEvent,
        on_click: Option<UnboundedSender<UiEvent>>,
    }

    impl Drop for LiveToast {
        fn drop(&mut self) {
            if let Err(err) = self.toast.RemoveActivated(self.token) {
                tracing::debug!(%err, "could not unregister the Windows toast handler");
            }
        }
    }

    fn show_toast(
        notifier: &mut Option<ToastNotifier>,
        activator: &mut Option<u32>,
        recent: &mut VecDeque<LiveToast>,
        tx: &mpsc::Sender<Command>,
        xml: &str,
        term_id: Option<TermId>,
        on_click: UnboundedSender<UiEvent>,
    ) -> anyhow::Result<()> {
        if activator.is_none() {
            ensure_registered().context("register Sich's toast identity")?;
            let factory: IClassFactory = ActivationFactory(tx.clone()).into();
            *activator = Some(unsafe {
                CoRegisterClassObject(
                    &ACTIVATOR,
                    &factory,
                    CLSCTX_LOCAL_SERVER,
                    REGCLS_MULTIPLEUSE,
                )?
            });
        }
        if notifier.is_none() {
            *notifier = Some(ToastNotificationManager::CreateToastNotifierWithId(
                &HSTRING::from(AUMID),
            )?);
        }
        let Some(notifier) = notifier else {
            return Ok(());
        };

        let tag = term_id.map_or_else(|| TEST_TAG.to_owned(), |id| id.to_string());
        let event = term_id.map_or(UiEvent::Show, UiEvent::FocusTerminal);
        let launch = format!("{:?}", unsafe { CoCreateGuid()? });
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        doc.DocumentElement()?
            .SetAttribute(&HSTRING::from("launch"), &HSTRING::from(&launch))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        toast.SetTag(&HSTRING::from(&tag))?;
        toast.SetGroup(&HSTRING::from(GROUP))?;

        let activate = tx.clone();
        let launch_arg = launch.clone();
        let token = toast.Activated(&TypedEventHandler::new(move |_, _| {
            let _ = activate.send(Command::Activate(launch_arg.clone()));
            Ok(())
        }))?;
        let entry = LiveToast {
            toast,
            token,
            tag,
            launch,
            event,
            on_click: Some(on_click),
        };
        notifier
            .Show(&entry.toast)
            .context("show native Windows toast")?;
        recent.retain(|old| old.tag != entry.tag);
        recent.push_back(entry);
        if recent.len() > MAX_RECENT_TOASTS {
            recent.pop_front();
        }
        Ok(())
    }

    fn remove_toast(tag: &str) -> windows::core::Result<()> {
        ToastNotificationManager::History()?.RemoveGroupedTagWithId(
            &HSTRING::from(tag),
            &HSTRING::from(GROUP),
            &HSTRING::from(AUMID),
        )
    }

    fn ensure_registered() -> anyhow::Result<()> {
        let dir = sich_daemon::data_local_dir()
            .context("no local data directory")?
            .join("sich");
        let icon = std::path::absolute(dir.join("toast-icon.ico"))?;
        let bytes = include_bytes!("../assets/icon.ico");
        if std::fs::read(&icon).as_deref().ok() != Some(bytes.as_slice()) {
            let dir = icon.parent().context("no toast icon directory")?;
            std::fs::create_dir_all(dir)?;
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            file.write_all(bytes)?;
            file.persist(&icon)?;
        }
        let key: Vec<u16> = format!(r"Software\Classes\AppUserModelId\{AUMID}")
            .encode_utf16()
            .chain([0])
            .collect();
        registry_string(&key, w!("DisplayName"), OsStr::new("Sich"))?;
        registry_string(&key, w!("IconUri"), icon.as_os_str())?;
        registry_string(
            &key,
            w!("CustomActivator"),
            OsStr::new(&format!("{{{ACTIVATOR:?}}}")),
        )?;
        Ok(())
    }

    fn registry_string(key: &[u16], name: PCWSTR, value: &OsStr) -> io::Result<()> {
        let value: Vec<u16> = value.encode_wide().chain([0]).collect();
        // RegSetKeyValueW creates the subkey if needed. The buffers are NUL-terminated UTF-16.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                size_of_val(value.as_slice()) as u32,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }

    // Notification Center activates an unpackaged app via COM. Register a class for this
    // running process and use the same launch argument as the per-toast Activated handler.
    #[implement(IClassFactory)]
    struct ActivationFactory(mpsc::Sender<Command>);

    impl IClassFactory_Impl for ActivationFactory_Impl {
        fn CreateInstance(
            &self,
            outer: Ref<IUnknown>,
            iid: *const GUID,
            object: *mut *mut c_void,
        ) -> windows::core::Result<()> {
            if !outer.is_null() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let callback: INotificationActivationCallback = Activator(self.0.clone()).into();
            // COM supplies the requested IID and a writable interface out-parameter.
            unsafe { callback.query(iid, object).ok() }
        }

        fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
            Ok(())
        }
    }

    #[implement(INotificationActivationCallback)]
    struct Activator(mpsc::Sender<Command>);

    impl INotificationActivationCallback_Impl for Activator_Impl {
        fn Activate(
            &self,
            app: &PCWSTR,
            launch: &PCWSTR,
            _data: *const NOTIFICATION_USER_INPUT_DATA,
            _count: u32,
        ) -> windows::core::Result<()> {
            if !app.is_null() && !launch.is_null() {
                // Windows supplies both arguments as valid NUL-terminated strings.
                let app = unsafe { app.to_string() };
                let launch = unsafe { launch.to_string() };
                if app.as_deref().ok() == Some(AUMID)
                    && let Ok(launch) = launch
                {
                    let _ = self.0.send(Command::Activate(launch));
                }
            }
            Ok(())
        }
    }
}
