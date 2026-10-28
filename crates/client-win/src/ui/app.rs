//! The GUI's start and the glue between `AppState` and the outside world: session events in,
//! effects out (session commands, gains, sounds, clipboard, settings file, trust store).

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use client_core::app::{Action, AppState, Effect, View};
use client_core::net::session::{SessionCommand, SessionEvent};
use client_core::settings::{Settings, SettingsStore};
use protocol::MemberId;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, PostMessageW, MB_ICONERROR, MB_ICONWARNING, MB_OK, MESSAGEBOX_STYLE,
};

use super::autostart::{is_elevated, relaunch_as_admin};
use super::sound::Sounds;
use super::clipboard;
use super::wide::wide;
use super::window::{Start, Window, WM_APP_SESSION};
use crate::dpapi::load_password;
use crate::startup::{default_data_dir, start_client, Account, Audio, Client};

pub struct Options {
    /// Start hidden, in the tray.
    pub minimized: bool,
    pub data_dir: Option<PathBuf>,
    /// Show window and tray, pump messages for a second, save settings and exit 0.
    pub smoke: bool,
}

/// Runs the GUI to the end; returns the process exit code.
pub fn run(opts: Options) -> i32 {
    let data_dir = opts.data_dir.clone().unwrap_or_else(default_data_dir);
    let settings_path = data_dir.join("settings.toml");
    let (store, settings, broken) = SettingsStore::load(&settings_path);
    // Games running as admin swallow hotkeys sent to a non-elevated process.
    if settings.run_as_admin && !opts.smoke && !is_elevated() && relaunch_as_admin() {
        return 0;
    }
    // Until the first-run dialog exists (Task 6).
    let account = settings.is_configured().then(|| account(&settings)).flatten();
    let Some(account) = account else {
        let mut text = format!("Brak konfiguracji.\n\nUzupełnij plik {}.", settings_path.display());
        if let Some(b) = broken {
            text.push_str(&format!("\n\nPoprzedni plik był uszkodzony i został odłożony jako {}.", b.display()));
        }
        message_box(None, &text, MB_ICONERROR);
        return 1;
    };

    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            message_box(None, &format!("Nie udało się uruchomić komunikatora: {e}"), MB_ICONERROR);
            return 1;
        }
    };
    let audio = Audio {
        capture: settings.capture_device.clone(),
        render: settings.render_device.clone(),
        sensitivity: settings.vad_sensitivity,
    };
    let (mut client, notices) = match start_client(rt.handle(), &data_dir, account, audio) {
        Ok(c) => c,
        Err(e) => {
            message_box(None, &format!("Nie udało się uruchomić komunikatora: {e:#}"), MB_ICONERROR);
            return 1;
        }
    };
    let mut session_events = std::mem::replace(&mut client.session.events, tokio::sync::mpsc::unbounded_channel().1);
    let (events_tx, events) = mpsc::channel();
    let mut app = App {
        state: AppState::with_clock(settings, Box::new(local_hm)),
        store,
        client: Some(client),
        events,
        sounds: Sounds::new(),
        speaking: Vec::new(),
        local_notice: None,
    };
    let mut warnings = Vec::new();
    for n in notices {
        if n.warning {
            warnings.push(n.text);
        } else {
            app.local_notice = Some(n.text);
        }
    }

    let window = match Window::create(app, Start { minimized: opts.minimized, smoke: opts.smoke }) {
        Ok(w) => w,
        Err(e) => {
            message_box(None, &format!("Nie udało się utworzyć okna: {e}"), MB_ICONERROR);
            return 1;
        }
    };
    // HWND is not Send; the forwarder only needs its value to post a wake-up.
    let hwnd_bits = window.hwnd().0 as usize;
    rt.spawn(async move {
        while let Some(e) = session_events.recv().await {
            if events_tx.send(e).is_err() {
                break;
            }
            let hwnd = HWND(hwnd_bits as *mut _);
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_SESSION, WPARAM(0), LPARAM(0)) };
        }
    });
    if !opts.smoke {
        for w in warnings {
            message_box(Some(window.hwnd()), &w, MB_ICONWARNING);
        }
    }

    let code = window.message_loop();
    window.close();
    window.save_settings();
    window.with_app(App::shutdown);
    // Stops the forwarder before the window goes, so nothing posts to a dead handle.
    rt.shutdown_timeout(Duration::from_secs(1));
    window.destroy();
    code
}

fn account(s: &Settings) -> Option<Account> {
    Some(Account {
        server: s.server.clone(),
        fingerprint: protocol::parse_fingerprint(&s.fingerprint)?,
        username: s.username.clone(),
        password: load_password(s)?,
    })
}

fn local_hm() -> (u8, u8) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wHour as u8, t.wMinute as u8)
}

/// A modal message box; runs a message loop, so never call it while borrowing window state.
pub fn message_box(owner: Option<HWND>, text: &str, icon: MESSAGEBOX_STYLE) {
    let (text, title) = (wide(text), wide("Komunikator"));
    unsafe { MessageBoxW(owner, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | icon) };
}

/// The model plus everything its effects act on.
pub struct App {
    state: AppState,
    store: SettingsStore,
    /// `None` after shutdown.
    client: Option<Client>,
    events: mpsc::Receiver<SessionEvent>,
    /// Purges playback on drop, so it lives as long as the window.
    sounds: Sounds,
    speaking: Vec<MemberId>,
    /// A status-bar message the model does not know about (hotkey or file errors).
    local_notice: Option<String>,
}

impl App {
    pub fn view(&self) -> &View {
        self.state.view()
    }

    pub fn view_version(&self) -> u64 {
        self.state.view_version()
    }

    pub fn settings(&self) -> &Settings {
        self.state.settings()
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        self.state.settings_mut()
    }

    pub fn set_local_notice(&mut self, n: String) {
        self.local_notice = Some(n);
    }

    pub fn take_local_notice(&mut self) -> Option<String> {
        self.local_notice.take()
    }

    pub fn act(&mut self, hwnd: HWND, a: Action) {
        let fx = self.state.on_action(a);
        self.run_effects(hwnd, fx);
    }

    /// Applies every session event queued by the forwarder.
    pub fn drain_events(&mut self, hwnd: HWND) {
        while let Ok(e) = self.events.try_recv() {
            if let (SessionEvent::MemberLeft(id), Some(c)) = (&e, &self.client) {
                // Frees the slot; the model only sets gains, it never removes them.
                c.gains.remove(*id);
            }
            let fx = self.state.on_event(e);
            self.run_effects(hwnd, fx);
        }
    }

    /// Reads who is speaking; true when the view changed.
    pub fn poll_activity(&mut self) -> bool {
        let Some(c) = &self.client else { return false };
        c.activity.speaking(&mut self.speaking);
        let me = c.activity.me_speaking();
        self.state.on_activity(&self.speaking, me)
    }

    pub fn save_settings(&mut self) {
        if let Err(e) = self.store.save(self.state.settings()) {
            tracing::warn!("settings not saved: {e}");
            self.local_notice = Some(format!("nie udało się zapisać ustawień: {e}"));
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(c) = self.client.take() {
            c.shutdown();
        }
    }

    fn run_effects(&mut self, hwnd: HWND, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::Command(cmd) => self.command(cmd),
                Effect::SetGain { id, gain } => {
                    if let Some(c) = &self.client {
                        if !c.gains.set(id, gain) {
                            self.local_notice = Some(
                                "brak miejsca w tabeli głośności — ustawienie zadziała po ponownym połączeniu".into(),
                            );
                        }
                    }
                }
                Effect::ClearGains => {
                    if let Some(c) = &self.client {
                        c.gains.clear();
                    }
                }
                Effect::PlaySound(k) => self.sounds.play(k),
                Effect::CopyToClipboard(text) => {
                    if !clipboard::set_text(hwnd, &text) {
                        self.local_notice = Some("schowek jest zajęty, spróbuj jeszcze raz".into());
                    }
                }
                Effect::SaveSettings => self.save_settings(),
                Effect::Trust { verify, id, name, identity } => {
                    let Some(c) = &self.client else { continue };
                    let failed = match c.trust.lock() {
                        Ok(mut store) => {
                            if verify {
                                store.verify(&name, &identity);
                            } else {
                                store.accept(&name, &identity);
                            }
                            store.save_failed()
                        }
                        Err(_) => true,
                    };
                    if failed {
                        self.local_notice = Some(
                            "nie udało się zapisać listy zaufanych urządzeń; zmiana obowiązuje do zamknięcia programu"
                                .into(),
                        );
                    }
                    self.command(SessionCommand::TrustChanged(id));
                }
            }
        }
        if let Some(c) = &self.client {
            use std::sync::atomic::Ordering::Relaxed;
            c.controls.muted.store(self.state.muted(), Relaxed);
            c.controls.deafened.store(self.state.deafened(), Relaxed);
        }
    }

    fn command(&self, cmd: SessionCommand) {
        if let Some(c) = &self.client {
            let _ = c.session.commands.send(cmd);
        }
    }
}
