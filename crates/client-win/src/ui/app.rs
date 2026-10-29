//! The GUI's start and the glue between `AppState` and the outside world: session events in,
//! effects out (session commands, gains, sounds, clipboard, settings file, trust store).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use client_core::app::{describe_fatal, Action, AppState, Effect, Status, View};
use client_core::net::session::{Credentials, FatalReason, SessionCommand, SessionEvent};
use client_core::settings::{Settings, SettingsStore};
use client_core::update::{self, current_version, may_apply, CheckError, Decision};
use protocol::invite::encode_invite;
use protocol::update::Version;
use protocol::MemberId;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, PostMessageW, MB_ICONERROR, MB_ICONWARNING, MB_OK, MESSAGEBOX_STYLE,
};

use super::autostart::{autostart_enabled, is_elevated, relaunch_as_admin, set_autostart};
use super::clipboard;
use super::dialogs::{
    try_account_dialog, AccountChoice, AccountForm, AccountInput, AccountMode, DialogUnavailable, Registration,
    SettingsInput,
};
use super::instance;
use super::sound::Sounds;
use super::wide::wide;
use super::window::{Start, Window, WM_APP_SESSION, WM_APP_UPDATE};
use crate::dpapi::{load_password, load_token, store_password, store_token};
use crate::startup::{default_data_dir, start_client, Account, Audio, Client, Notice};
use crate::updater;

pub struct Options {
    /// Start hidden, in the tray.
    pub minimized: bool,
    pub data_dir: Option<PathBuf>,
    /// Show window and tray, pump messages for a second, save settings and exit 0.
    pub smoke: bool,
    /// Started by a self-update in place of the previous version.
    pub updated: bool,
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
    // Before anything else: a swap cut short leaves no exe under the name autostart uses.
    // The self-test never touches the exe next to it.
    let updates = if opts.smoke { None } else { Updates::new() };
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
    let mut register = None;
    if account(&settings, None).is_none() {
        // The self-test never waits for a person.
        if opts.smoke {
            return 1;
        }
        let choice = match try_account_dialog(None, &account_form(&settings), true) {
            Ok(Some(c)) => c,
            Ok(None) => return 0,
            Err(DialogUnavailable) => {
                message_box(None, "Nie udało się otworzyć okna z danymi konta.", MB_ICONERROR);
                return 1;
            }
        };
        register = match set_account(&mut settings, choice) {
            Ok(r) => r,
            Err(e) => {
                message_box(None, &format!("Nie udało się zapisać hasła: {e:#}"), MB_ICONERROR);
                return 1;
            }
        };
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
    let mut startup_notice = opts.updated.then(|| format!("{UPDATED_TO}{}", current_version()));
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
        register,
        login_prompt: None,
        updates,
    };

    let window = match Window::create(app, Start { minimized: opts.minimized, smoke: opts.smoke }) {
        Ok(w) => w,
        Err(e) => {
            message_box(None, &format!("Nie udało się utworzyć okna: {e}"), MB_ICONERROR);
            return 1;
        }
    };
    // Before the session: a client too old for the server still finds its update.
    window.with_app(|app| app.check_update(window.hwnd()));
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
    let mut restart = None;
    window.with_app(|app| restart = app.installed_update());
    window.destroy();
    if let Some(exe) = restart {
        // The new copy must find the single-instance name free.
        _guard = None;
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        if let Err(e) = updater::restart(&exe, &args) {
            message_box(
                None,
                &format!(
                    "Komunikator został zaktualizowany, ale nie udało się go uruchomić ponownie ({e}). \
                     Uruchom go sam."
                ),
                MB_ICONWARNING,
            );
        }
    }
    code
}

const UPDATE_DOWNLOADING: &str = "pobieranie aktualizacji…";
const UPDATE_READY: &str = "aktualizacja gotowa — zostanie zainstalowana, gdy nie będziesz w rozmowie";
const UPDATE_FAILED: &str = "aktualizacja nieudana: ";
const UPDATED_TO: &str = "zaktualizowano do wersji ";

/// What a background update check reports back to the window.
enum UpdateMsg {
    Downloading,
    UpToDate,
    Ready(Version),
    Failed(UpdateFailure),
}

#[derive(Debug)]
enum UpdateFailure {
    /// The server could not be asked (offline, or too old for the update channel).
    Unreachable(String),
    /// A release was found but could not be verified, downloaded or installed.
    Failed(String),
}

/// The status-bar text for a failed update, if any: nothing when the server simply could not
/// be asked (that would nag at every start offline), and never over a notice that is not
/// about updates (`shown`).
fn failure_notice(f: &UpdateFailure, shown: Option<&str>) -> Option<String> {
    let UpdateFailure::Failed(why) = f else { return None };
    let about_updates = |n: &str| {
        n == UPDATE_DOWNLOADING || n == UPDATE_READY || n.starts_with(UPDATE_FAILED) || n.starts_with(UPDATED_TO)
    };
    shown.is_none_or(about_updates).then(|| format!("{UPDATE_FAILED}{why}"))
}

/// Hands update reports to the window thread and wakes it.
struct UpdatePost {
    tx: mpsc::Sender<UpdateMsg>,
    /// HWND is not Send; posting only needs its value.
    hwnd: usize,
}

impl UpdatePost {
    fn send(&self, m: UpdateMsg) {
        if self.tx.send(m).is_ok() {
            let hwnd = HWND(self.hwnd as *mut _);
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_UPDATE, WPARAM(0), LPARAM(0)) };
        }
    }
}

/// Self-update state; absent when this build has no release key (or in the self-test).
struct Updates {
    key: [u8; 32],
    exe: PathBuf,
    tx: mpsc::Sender<UpdateMsg>,
    rx: mpsc::Receiver<UpdateMsg>,
    /// A check or download is running.
    busy: bool,
    /// Staged beside the exe, waiting until I am not in a conversation.
    ready: Option<Version>,
    /// Swapped in: the new exe starts once this one has closed.
    installed: bool,
}

impl Updates {
    fn new() -> Option<Updates> {
        let exe = match std::env::current_exe() {
            Ok(exe) => updater::canonical_exe(&exe),
            Err(e) => {
                tracing::warn!("self-update disabled: no exe path: {e}");
                return None;
            }
        };
        updater::cleanup_on_start(&exe);
        if let Err(e) = updater::write_version_stamp(&exe, current_version()) {
            tracing::debug!("version stamp not written: {e}");
        }
        let Some(key) = updater::release_key() else {
            tracing::info!("self-update disabled: this build has no release key");
            return None;
        };
        let (tx, rx) = mpsc::channel();
        Some(Updates { key, exe, tx, rx, busy: false, ready: None, installed: false })
    }
}

/// Checks the server for a newer signed release and stages it; the version staged, if any.
async fn fetch_update(
    server: &str,
    fingerprint: [u8; 32],
    key: &[u8; 32],
    exe: &Path,
    post: &UpdatePost,
) -> Result<Option<Version>, UpdateFailure> {
    let decision = update::check(server, fingerprint, key, current_version()).await.map_err(|e| match e {
        CheckError::Unreachable(e) => UpdateFailure::Unreachable(format!("{e:#}")),
        CheckError::Rejected(why) => UpdateFailure::Failed(why.into()),
    })?;
    let Decision::Available(m) = decision else { return Ok(None) };
    post.send(UpdateMsg::Downloading);
    let bytes = update::download(server, fingerprint, &m).await.map_err(|e| UpdateFailure::Failed(format!("{e}")))?;
    let exe = exe.to_owned();
    tokio::task::spawn_blocking(move || updater::stage(&exe, &bytes))
        .await
        .map_err(|_| UpdateFailure::Failed("przerwane zapisywanie aktualizacji".into()))?
        .map_err(|e| UpdateFailure::Failed(updater::describe_io(&e).into()))?;
    Ok(Some(m.version))
}

/// What to log in with: a registration in progress, else the saved token, else the saved
/// password.
fn account(s: &Settings, register: Option<&Registration>) -> Option<Account> {
    if let Some(r) = register {
        return Some(Account {
            server: r.invite.server.clone(),
            fingerprint: r.invite.fingerprint,
            username: r.username.clone(),
            credentials: Credentials::Register { invite: r.invite.code, password: r.password.clone() },
        });
    }
    if !s.is_configured() {
        return None;
    }
    let credentials = match load_token(s) {
        Some(t) => Credentials::Token(t),
        None => Credentials::Password(load_password(s)?),
    };
    Some(Account {
        server: s.server.clone(),
        fingerprint: protocol::parse_fingerprint(&s.fingerprint)?,
        username: s.username.clone(),
        credentials,
    })
}

/// The account form's starting values; the password only if it can still be decrypted (with
/// a stored session there is none, and it may be left empty).
pub fn account_form(s: &Settings) -> AccountForm {
    AccountForm {
        // Nothing set up yet most likely means a friend's invite.
        mode: if s.server.is_empty() { AccountMode::Invite } else { AccountMode::Login },
        login: AccountInput {
            server: s.server.clone(),
            fingerprint: s.fingerprint.clone(),
            username: s.username.clone(),
            password: load_password(s).unwrap_or_default(),
        },
        invite: String::new(),
        invite_user: String::new(),
        notice: None,
        remembered: load_token(s).is_some(),
    }
}

/// Whether the account dialog returned the saved account as it is: same server, fingerprint
/// and name, and the saved password or none (meaning the stored session).
fn unchanged_login(s: &Settings, a: &AccountInput) -> bool {
    s.server == a.server
        && s.fingerprint == a.fingerprint
        && s.username == a.username
        && (a.password.is_empty() || load_password(s).as_deref() == Some(a.password.as_str()))
}

/// Keeps a freshly issued token for the next start. The first one of a registration also makes
/// the new account the saved one, replacing the old account's credentials only now that the
/// registration has succeeded.
fn adopt_token(s: &mut Settings, token: &[u8; 32], register: Option<Registration>) -> anyhow::Result<()> {
    store_token(s, token)?;
    if let Some(r) = register {
        s.server = r.invite.server;
        s.fingerprint = protocol::fingerprint_hex(&r.invite.fingerprint);
        s.username = r.username;
    }
    Ok(())
}

/// Saves my name as the server spells it; true when it differed from the one typed.
fn signed_in_as(s: &mut Settings, name: &str) -> bool {
    if s.username == name {
        return false;
    }
    s.username = name.to_owned();
    true
}

/// The form to reopen after a login failed for good, or `None` when the account is not the
/// problem. Registration errors reopen the invite form only while registering.
fn login_prompt(s: &Settings, reason: FatalReason, register: Option<&Registration>) -> Option<AccountForm> {
    let mut form = account_form(s);
    form.notice = Some(describe_fatal(reason));
    match reason {
        FatalReason::LoginRequired | FatalReason::BadCredentials => form.mode = AccountMode::Login,
        FatalReason::InviteInvalid
        | FatalReason::NameTaken
        | FatalReason::NameInvalid
        | FatalReason::PasswordTooShort => {
            let r = register?;
            form.mode = AccountMode::Invite;
            if reason != FatalReason::InviteInvalid {
                form.invite = encode_invite(&r.invite);
            }
            form.invite_user = r.username.clone();
        }
        FatalReason::VersionMismatch { .. } | FatalReason::SessionReplaced | FatalReason::AccountBlocked => {
            return None
        }
    }
    Some(form)
}

/// Saves the dialog's account to the settings. A login's password is stored protected (an
/// empty one, allowed only for the saved account, keeps its stored session). A registration
/// changes nothing yet: it is returned, used once, and saved with its first token
/// (`adopt_token`), so a refused one leaves the old account working.
fn set_account(s: &mut Settings, choice: AccountChoice) -> anyhow::Result<Option<Registration>> {
    match choice {
        AccountChoice::Login(a) => {
            if !a.password.is_empty() {
                store_password(s, &a.password)?;
            }
            s.server = a.server;
            s.fingerprint = a.fingerprint;
            s.username = a.username;
            Ok(None)
        }
        AccountChoice::Register(r) => Ok(Some(r)),
    }
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
    /// An invite registration not yet confirmed by a token; kept only in memory.
    register: Option<Registration>,
    /// The account form a failed login asks the window to open.
    login_prompt: Option<AccountForm>,
    updates: Option<Updates>,
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
        let account = account(s, self.register.as_ref())
            .ok_or_else(|| "brak konta albo nie da się odczytać zapisanego hasła".to_owned())?;
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
    pub fn change_account(&mut self, hwnd: HWND, choice: AccountChoice) {
        let same = match &choice {
            AccountChoice::Login(a) => unchanged_login(self.state.settings(), a),
            AccountChoice::Register(_) => false,
        };
        if !needs_reconnect(same, &self.state.view().status) {
            return;
        }
        match set_account(self.state.settings_mut(), choice) {
            Ok(r) => self.register = r,
            Err(e) => {
                self.local_notice = Some(format!("nie udało się zapisać hasła: {e:#}"));
                return;
            }
        }
        self.save_settings();
        self.state.reset_session();
        // A prompt still waiting was about the account just replaced.
        self.login_prompt = None;
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

    /// The account form a failed login asked for, once.
    pub fn take_login_prompt(&mut self) -> Option<AccountForm> {
        self.login_prompt.take()
    }

    /// Applies every session event queued by the forwarder.
    pub fn drain_events(&mut self, hwnd: HWND) {
        while let Ok((generation, e)) = self.events.try_recv() {
            if generation != self.generation {
                continue;
            }
            match &e {
                // Frees the slot; the model only sets gains, it never removes them.
                SessionEvent::MemberLeft(id) => {
                    if let Some(c) = &self.client {
                        c.gains.remove(*id);
                    }
                }
                SessionEvent::TokenIssued(token) => self.token_issued(&token.0),
                SessionEvent::SignedInAs(name) => {
                    if signed_in_as(self.state.settings_mut(), name) {
                        self.save_settings();
                    }
                }
                SessionEvent::Fatal(reason) => {
                    self.login_prompt = login_prompt(self.state.settings(), *reason, self.register.as_ref());
                }
                _ => {}
            }
            let fx = self.state.on_event(e);
            self.run_effects(hwnd, fx);
        }
    }

    /// Looks for a newer release in the background, unless one is already being fetched or
    /// waits to be installed.
    pub fn check_update(&mut self, hwnd: HWND) {
        let Some(u) = &mut self.updates else { return };
        if u.busy || u.ready.is_some() || u.installed {
            return;
        }
        let s = self.state.settings();
        let target = match &self.register {
            Some(r) => Some((r.invite.server.clone(), r.invite.fingerprint)),
            None => protocol::parse_fingerprint(&s.fingerprint).map(|fp| (s.server.clone(), fp)),
        };
        let Some((server, fingerprint)) = target.filter(|(server, _)| !server.is_empty()) else { return };
        u.busy = true;
        let post = UpdatePost { tx: u.tx.clone(), hwnd: hwnd.0 as usize };
        let (key, exe) = (u.key, u.exe.clone());
        self.rt.spawn(async move {
            let msg = match fetch_update(&server, fingerprint, &key, &exe, &post).await {
                Ok(None) => UpdateMsg::UpToDate,
                Ok(Some(v)) => UpdateMsg::Ready(v),
                Err(f) => UpdateMsg::Failed(f),
            };
            post.send(msg);
        });
    }

    /// Applies what the background update check reported; `shown` is the status-bar notice.
    pub fn drain_updates(&mut self, shown: Option<&str>) {
        let Some(u) = &mut self.updates else { return };
        while let Ok(m) = u.rx.try_recv() {
            match m {
                UpdateMsg::Downloading => self.local_notice = Some(UPDATE_DOWNLOADING.into()),
                UpdateMsg::UpToDate => u.busy = false,
                UpdateMsg::Ready(v) => {
                    u.busy = false;
                    u.ready = Some(v);
                    self.local_notice = Some(UPDATE_READY.into());
                }
                UpdateMsg::Failed(f) => {
                    u.busy = false;
                    match &f {
                        UpdateFailure::Unreachable(why) => tracing::info!("update server not reachable: {why}"),
                        UpdateFailure::Failed(why) => tracing::warn!("update failed: {why}"),
                    }
                    if let Some(n) = failure_notice(&f, self.local_notice.as_deref().or(shown)) {
                        self.local_notice = Some(n);
                    }
                }
            }
        }
    }

    /// Puts a staged release in place of the exe once I am not in a conversation; true when
    /// the program should now close (`run` then starts the new exe).
    pub fn install_update_if_due(&mut self, shown: Option<&str>) -> bool {
        let Some(u) = &mut self.updates else { return false };
        if u.ready.is_none() || !may_apply(self.state.in_room_with_others()) {
            return false;
        }
        u.ready = None;
        match updater::swap(&u.exe) {
            Ok(()) => {
                u.installed = true;
                true
            }
            Err(e) => {
                tracing::warn!("update swap failed: {e}");
                let f = UpdateFailure::Failed(updater::describe_io(&e).into());
                if let Some(n) = failure_notice(&f, self.local_notice.as_deref().or(shown)) {
                    self.local_notice = Some(n);
                }
                false
            }
        }
    }

    /// The exe to start after closing, when an update was installed.
    fn installed_update(&self) -> Option<PathBuf> {
        self.updates.as_ref().filter(|u| u.installed).map(|u| u.exe.clone())
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

    /// Keeps the token for the next start in place of the password (and of the invite).
    fn token_issued(&mut self, token: &[u8; 32]) {
        let register = self.register.take();
        match adopt_token(self.state.settings_mut(), token, register) {
            Ok(()) => self.save_settings(),
            Err(e) => self.local_notice = Some(format!("nie udało się zapamiętać logowania: {e:#}")),
        }
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
                Effect::CheckUpdate => self.check_update(hwnd),
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

    fn registration() -> Registration {
        let invite = protocol::invite::Invite { server: "serwer.pl:7700".into(), fingerprint: [0xAB; 32], code: [5; 16] };
        Registration { invite, username: "Ola2".into(), password: "długie hasło".into() }
    }

    fn logged_in() -> Settings {
        let mut s = Settings { server: "stary.pl:7700".into(), fingerprint: "cd".repeat(32), username: "Ola".into(), ..Settings::default() };
        store_token(&mut s, &[3; 32]).unwrap();
        s
    }

    #[test]
    fn registration_keeps_the_old_account_until_its_first_token() {
        let mut s = logged_in();
        let before = s.clone();
        let r = set_account(&mut s, AccountChoice::Register(registration())).unwrap().unwrap();
        assert_eq!(s, before, "a registration that may still fail must not touch the saved account");
        let a = account(&s, Some(&r)).unwrap();
        assert_eq!(a.credentials, Credentials::Register { invite: [5; 16], password: "długie hasło".into() });
        assert_eq!((a.server.as_str(), a.username.as_str()), ("serwer.pl:7700", "Ola2"));

        // Refused: the old account still logs in.
        assert_eq!(account(&s, None).unwrap().credentials, Credentials::Token([3; 32]));

        // The first token makes the new account the saved one.
        adopt_token(&mut s, &[7; 32], Some(r)).unwrap();
        assert_eq!((s.server.as_str(), s.username.as_str()), ("serwer.pl:7700", "Ola2"));
        assert_eq!(s.fingerprint, "ab".repeat(32));
        assert!(s.password_protected.is_empty());
        assert_eq!(account(&s, None).unwrap().credentials, Credentials::Token([7; 32]));
        adopt_token(&mut s, &[8; 32], None).unwrap();
        assert_eq!(account(&s, None).unwrap().credentials, Credentials::Token([8; 32]));
        assert_eq!(s.username, "Ola2");
    }

    #[test]
    fn the_servers_spelling_of_the_name_is_saved() {
        let mut s = logged_in();
        assert!(!signed_in_as(&mut s, "Ola"));
        assert!(signed_in_as(&mut s, "OLA"));
        assert_eq!(s.username, "OLA");
    }

    #[test]
    fn empty_password_reconnects_with_the_remembered_session() {
        let s = logged_in();
        let form = account_form(&s);
        assert!(form.remembered && form.login.password.is_empty());
        let same = AccountInput {
            server: s.server.clone(),
            fingerprint: s.fingerprint.clone(),
            username: s.username.clone(),
            password: String::new(),
        };
        assert!(unchanged_login(&s, &same));
        assert!(!unchanged_login(&s, &AccountInput { password: "nowe hasło".into(), ..same.clone() }));
        assert!(!unchanged_login(&s, &AccountInput { username: "Ewa".into(), ..same.clone() }));
        // Saving it keeps the token instead of storing an empty password.
        let mut saved = s.clone();
        assert_eq!(set_account(&mut saved, AccountChoice::Login(same)).unwrap(), None);
        assert_eq!(account(&saved, None).unwrap().credentials, Credentials::Token([3; 32]));
        // Password-only settings have no session to remember.
        let mut pw = Settings { token_protected: String::new(), ..s };
        store_password(&mut pw, "hasło").unwrap();
        assert!(!account_form(&pw).remembered);
    }

    #[test]
    fn account_errors_reopen_the_right_form() {
        let mut s = Settings { server: "serwer.pl:7700".into(), username: "ola".into(), ..Settings::default() };
        store_token(&mut s, &[7; 32]).unwrap();
        for reason in [FatalReason::LoginRequired, FatalReason::BadCredentials] {
            let form = login_prompt(&s, reason, None).unwrap();
            assert_eq!(form.mode, AccountMode::Login);
            assert_eq!((form.login.server.as_str(), form.login.username.as_str()), ("serwer.pl:7700", "ola"));
            assert_eq!(form.notice, Some(describe_fatal(reason)));
        }
        let r = registration();
        let form = login_prompt(&s, FatalReason::NameTaken, Some(&r)).unwrap();
        assert_eq!(form.mode, AccountMode::Invite);
        assert_eq!(form.invite, encode_invite(&r.invite));
        assert_eq!(form.invite_user, "Ola2");
        assert_eq!(login_prompt(&s, FatalReason::InviteInvalid, Some(&r)).unwrap().invite, "");
        assert_eq!(login_prompt(&s, FatalReason::NameTaken, None), None);
        for reason in [FatalReason::AccountBlocked, FatalReason::SessionReplaced, FatalReason::VersionMismatch { server: 9 }] {
            assert_eq!(login_prompt(&s, reason, Some(&r)), None);
        }
    }

    #[test]
    fn update_failures_show_only_when_a_release_was_found_and_never_over_other_notices() {
        let offline = UpdateFailure::Unreachable("brak połączenia z serwerem".into());
        assert_eq!(failure_notice(&offline, None), None, "offline is not worth a notice");
        let bad = UpdateFailure::Failed("opis wydania nie ma poprawnego podpisu".into());
        let text = Some("aktualizacja nieudana: opis wydania nie ma poprawnego podpisu".to_owned());
        assert_eq!(failure_notice(&bad, None), text);
        for ours in [UPDATE_DOWNLOADING, UPDATE_READY, "aktualizacja nieudana: x", "zaktualizowano do wersji 1.2.3"] {
            assert_eq!(failure_notice(&bad, Some(ours)), text, "{ours}");
        }
        assert_eq!(failure_notice(&bad, Some("skrót: Wycisz mikrofon (Ctrl+M): zajęty")), None);
    }

    #[test]
    fn unchanged_account_reconnects_only_after_a_fatal_error() {
        assert!(!needs_reconnect(true, &Status::Connected));
        assert!(!needs_reconnect(true, &Status::Connecting));
        assert!(!needs_reconnect(true, &Status::Disconnected { retry_in_s: 3 }));
        assert!(needs_reconnect(true, &Status::Fatal("zalogowano się na to konto w innym miejscu".into())));
        assert!(needs_reconnect(false, &Status::Connected));
    }
}
