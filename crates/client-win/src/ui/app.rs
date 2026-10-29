//! The GUI's start and the glue between `AppState` and the outside world: session events in,
//! effects out (session commands, gains, sounds, clipboard, settings file, trust store).

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use client_core::app::{Action, AppState, Effect, Status, View};
use client_core::net::session::{SessionCommand, SessionEvent};
use client_core::settings::{Settings, SettingsStore};
use protocol::MemberId;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, PostMessageW, MB_ICONERROR, MB_ICONWARNING, MB_OK, MESSAGEBOX_STYLE,
};

use super::autostart::{autostart_enabled, is_elevated, relaunch_as_admin, set_autostart};
use super::clipboard;
use super::dialogs::{try_account_dialog, AccountInput, DialogUnavailable, SettingsInput};
use super::instance;
use super::sound::Sounds;
use super::wide::wide;
use super::window::{Start, Window, WM_APP_SESSION};
use crate::dpapi::{load_password, store_password};
use crate::startup::{default_data_dir, start_client, Account, Audio, Client, Notice};

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
    // The self-test runs beside a real copy; a second real start shows the first one instead.
    let claim = || if opts.smoke { Some(None) } else { instance::claim(&data_dir).map(Some) };
    let Some(mut _guard) = claim() else { return 0 };
    // Games running as admin swallow hotkeys sent to a non-elevated process.
    if settings.run_as_admin && !opts.smoke && !is_elevated() {
        // The elevated copy must find the name free; if UAC is declined, carry on as we are.
        _guard = None;
        if relaunch_as_admin() {
            return 0;
        }
        let Some(g) = claim() else { return 0 };
        _guard = g;
    }
    let mut settings = settings;
    if let Some(b) = broken.filter(|_| !opts.smoke) {
        let text = match b {
            Ok(p) => format!("Plik ustawień był uszkodzony i został odłożony jako {}.", p.display()),
            Err(why) => format!(
                "Plik ustawień był uszkodzony i nie udało się go odłożyć ({why}).\n\
                 Zostanie zastąpiony ustawieniami domyślnymi."
            ),
        };
        message_box(None, &text, MB_ICONWARNING);
    }
    if account(&settings).is_none() {
        // The self-test never waits for a person.
        if opts.smoke {
            return 1;
        }
        let a = match try_account_dialog(None, &account_input(&settings), true) {
            Ok(Some(a)) => a,
            Ok(None) => return 0,
            Err(DialogUnavailable) => {
                message_box(None, "Nie udało się otworzyć okna z danymi konta.", MB_ICONERROR);
                return 1;
            }
        };
        if let Err(e) = set_account(&mut settings, &a) {
            message_box(None, &format!("Nie udało się zapisać hasła: {e:#}"), MB_ICONERROR);
            return 1;
        }
        if let Err(e) = store.save(&settings) {
            message_box(None, &format!("Nie udało się zapisać ustawień: {e}"), MB_ICONWARNING);
        }
    }

    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            message_box(None, &format!("Nie udało się uruchomić komunikatora: {e}"), MB_ICONERROR);
            return 1;
        }
    };
    // The exe may have moved since the entry was written (an update, another folder).
    let mut startup_notice = None;
    if settings.autostart && !opts.smoke {
        if let Err(e) = set_autostart(true) {
            startup_notice = Some(format!("nie udało się odświeżyć uruchamiania z Windows: {e}"));
        }
    }
    let (events_tx, events) = mpsc::channel();
    let app = App {
        state: AppState::with_clock(settings, Box::new(local_hm)),
        store,
        client: None,
        events,
        events_tx,
        generation: 0,
        rt: rt.handle().clone(),
        data_dir,
        sounds: Sounds::new(),
        speaking: Vec::new(),
        local_notice: startup_notice,
        playback_lost_shown: false,
    };

    let window = match Window::create(app, Start { minimized: opts.minimized, smoke: opts.smoke }) {
        Ok(w) => w,
        Err(e) => {
            message_box(None, &format!("Nie udało się utworzyć okna: {e}"), MB_ICONERROR);
            return 1;
        }
    };
    let mut started = Err(String::new());
    window.with_app(|app| started = app.connect(window.hwnd()));
    let notices = match started {
        Ok(n) => n,
        Err(e) => {
            window.close();
            message_box(None, &format!("Nie udało się uruchomić komunikatora: {e}"), MB_ICONERROR);
            window.destroy();
            return 1;
        }
    };
    let mut warnings = Vec::new();
    window.with_app(|app| {
        for n in notices {
            if n.warning {
                warnings.push(n.text);
            } else {
                app.local_notice = Some(n.text);
            }
        }
    });
    window.redraw();
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
    if !s.is_configured() {
        return None;
    }
    Some(Account {
        server: s.server.clone(),
        fingerprint: protocol::parse_fingerprint(&s.fingerprint)?,
        username: s.username.clone(),
        password: load_password(s)?,
    })
}

/// The account form's starting values; the password only if it can still be decrypted.
pub fn account_input(s: &Settings) -> AccountInput {
    AccountInput {
        server: s.server.clone(),
        fingerprint: s.fingerprint.clone(),
        username: s.username.clone(),
        password: load_password(s).unwrap_or_default(),
    }
}

fn set_account(s: &mut Settings, a: &AccountInput) -> anyhow::Result<()> {
    store_password(s, &a.password)?;
    s.server = a.server.clone();
    s.fingerprint = a.fingerprint.clone();
    s.username = a.username.clone();
    Ok(())
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

const PLAYBACK_LOST: &str = "błąd odtwarzania dźwięku — uruchom program ponownie";

/// Whether OK in the account dialog should start a new session.
fn needs_reconnect(same_account: bool, status: &Status) -> bool {
    !same_account || matches!(status, Status::Fatal(_))
}

/// The model plus everything its effects act on.
pub struct App {
    state: AppState,
    store: SettingsStore,
    /// `None` after shutdown.
    client: Option<Client>,
    /// Session events tagged with the session generation they came from.
    events: mpsc::Receiver<(u64, SessionEvent)>,
    events_tx: mpsc::Sender<(u64, SessionEvent)>,
    /// Bumped on every (re)start, so a replaced session's late events are dropped.
    generation: u64,
    rt: tokio::runtime::Handle,
    data_dir: PathBuf,
    /// Purges playback on drop, so it lives as long as the window.
    sounds: Sounds,
    speaking: Vec<MemberId>,
    /// A status-bar message the model does not know about (hotkey or file errors).
    local_notice: Option<String>,
    /// The "playback broke" notice was shown for the current client.
    playback_lost_shown: bool,
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

    /// The current client's microphone level (the client changes with the account).
    pub fn mic_level_dbfs(&self) -> f32 {
        self.client.as_ref().map_or(f32::NEG_INFINITY, |c| c.activity.mic_level_dbfs())
    }

    /// Starts (or restarts) the session and audio for the account in the settings, with a
    /// forwarder that wakes the window for each event.
    pub fn connect(&mut self, hwnd: HWND) -> Result<Vec<Notice>, String> {
        if let Some(c) = self.client.take() {
            c.shutdown();
        }
        self.generation += 1;
        let s = self.state.settings();
        let account = account(s).ok_or_else(|| "brak konta albo nie da się odczytać zapisanego hasła".to_owned())?;
        let audio =
            Audio { capture: s.capture_device.clone(), render: s.render_device.clone(), sensitivity: s.vad_sensitivity };
        let (mut client, notices) =
            start_client(&self.rt, &self.data_dir, account, audio).map_err(|e| format!("{e:#}"))?;
        let mut session_events =
            std::mem::replace(&mut client.session.events, tokio::sync::mpsc::unbounded_channel().1);
        let (tx, generation) = (self.events_tx.clone(), self.generation);
        // HWND is not Send; the forwarder only needs its value to post a wake-up.
        let hwnd_bits = hwnd.0 as usize;
        self.rt.spawn(async move {
            while let Some(e) = session_events.recv().await {
                if tx.send((generation, e)).is_err() {
                    break;
                }
                let hwnd = HWND(hwnd_bits as *mut _);
                let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_SESSION, WPARAM(0), LPARAM(0)) };
            }
        });
        self.client = Some(client);
        self.playback_lost_shown = false;
        self.sync_controls();
        Ok(notices)
    }

    /// Saves a changed account and reconnects with it. An unchanged one does nothing, unless
    /// the session has failed for good: then OK is the way to try again.
    pub fn change_account(&mut self, hwnd: HWND, a: AccountInput) {
        let s = self.state.settings();
        let same = s.server == a.server
            && s.fingerprint == a.fingerprint
            && s.username == a.username
            && load_password(s).as_deref() == Some(a.password.as_str());
        if !needs_reconnect(same, &self.state.view().status) {
            return;
        }
        if let Err(e) = set_account(self.state.settings_mut(), &a) {
            self.local_notice = Some(format!("nie udało się zapisać hasła: {e:#}"));
            return;
        }
        self.save_settings();
        self.state.reset_session();
        match self.connect(hwnd) {
            Ok(notices) => {
                if let Some(n) = notices.into_iter().last() {
                    self.local_notice = Some(n.text);
                }
            }
            // The old client is gone; "łączenie…" would wait forever.
            Err(e) => self.state.session_failed(format!("nie udało się połączyć: {e}")),
        }
    }

    /// Applies and saves what the settings dialog returned.
    pub fn apply_settings(&mut self, new: SettingsInput) {
        let old = SettingsInput::from_settings(self.state.settings());
        if let Some(c) = &mut self.client {
            if new.capture != old.capture || new.render != old.render {
                c.engine.restart_devices(new.capture.clone(), new.render.clone());
            }
            if new.sensitivity != old.sensitivity {
                c.engine.set_sensitivity(new.sensitivity);
            }
        }
        let mut notices = Vec::new();
        // Compared with the registry, so an entry removed elsewhere comes back.
        if new.autostart != autostart_enabled() {
            if let Err(e) = set_autostart(new.autostart) {
                notices.push(format!("nie udało się zmienić uruchamiania z Windows: {e}"));
            }
        }
        if new.run_as_admin && !old.run_as_admin && !is_elevated() {
            notices.push("uprawnienia administratora zadziałają po ponownym uruchomieniu komunikatora".to_owned());
        }
        new.apply_to(self.state.settings_mut());
        self.save_settings();
        if !notices.is_empty() {
            self.local_notice = Some(notices.join("; "));
        }
    }

    pub fn act(&mut self, hwnd: HWND, a: Action) {
        let fx = self.state.on_action(a);
        self.run_effects(hwnd, fx);
    }

    /// Applies every session event queued by the forwarder.
    pub fn drain_events(&mut self, hwnd: HWND) {
        while let Ok((generation, e)) = self.events.try_recv() {
            if generation != self.generation {
                continue;
            }
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
        let Some(c) = &mut self.client else { return false };
        let lost = c.engine.playback_lost() && !self.playback_lost_shown;
        c.activity.speaking(&mut self.speaking);
        let me = c.activity.me_speaking();
        if lost {
            self.playback_lost_shown = true;
            self.local_notice = Some(PLAYBACK_LOST.to_owned());
        }
        self.state.on_activity(&self.speaking, me) || lost
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
        self.sync_controls();
    }

    /// Hands the mute state to the audio threads.
    fn sync_controls(&self) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_account_reconnects_only_after_a_fatal_error() {
        assert!(!needs_reconnect(true, &Status::Connected));
        assert!(!needs_reconnect(true, &Status::Connecting));
        assert!(!needs_reconnect(true, &Status::Disconnected { retry_in_s: 3 }));
        assert!(needs_reconnect(true, &Status::Fatal("zalogowano się na to konto w innym miejscu".into())));
        assert!(needs_reconnect(false, &Status::Connected));
    }
}
