//! Shared start of a client (CLI and GUI): device identity, trust store, session and audio.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use client_core::audio::activity::Activity;
use client_core::audio::volume::GainTable;
use client_core::e2e::KnownDevices;
use client_core::net::session::{spawn_session, SessionHandle, SessionParams};

use crate::engine::AudioEngine;
use crate::identity_store::{load_or_create_identity, IdentityStatus};
use crate::pipeline::Controls;

/// `%APPDATA%\komunikator`, or the temp dir when APPDATA is missing.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("komunikator")
}

pub struct Account {
    /// "host:port"
    pub server: String,
    pub fingerprint: [u8; 32],
    pub username: String,
    pub password: String,
}

/// Something the user should read; `warning` ones are about trust and deserve attention.
pub struct Notice {
    pub text: String,
    pub warning: bool,
}

pub struct Audio {
    pub capture: Option<String>,
    pub render: Option<String>,
    pub sensitivity: f32,
}

/// A running client. `session.audio_in` is owned by the engine; dropping `session` stops it.
pub struct Client {
    pub session: SessionHandle,
    pub engine: AudioEngine,
    pub trust: Arc<Mutex<KnownDevices>>,
    pub controls: Arc<Controls>,
    pub gains: Arc<GainTable>,
    pub activity: Arc<Activity>,
}

impl Client {
    pub fn shutdown(self) {
        self.session.shutdown();
        self.engine.stop();
    }
}

/// Loads (or creates) the identity and trust store from `data_dir`, then starts the session
/// and the audio engine. The notices are for the user, in Polish, in order.
pub fn start_client(
    rt: &tokio::runtime::Handle,
    data_dir: &Path,
    account: Account,
    audio: Audio,
) -> anyhow::Result<(Client, Vec<Notice>)> {
    let mut notices = Vec::new();
    let (identity, status) = load_or_create_identity(data_dir)?;
    match status {
        IdentityStatus::Loaded => {}
        IdentityStatus::Created => notices.push(Notice {
            text: "utworzono nową tożsamość urządzenia (klucz szyfrowania)".to_owned(),
            warning: false,
        }),
        IdentityStatus::Replaced { broken } => notices.push(warning(format!(
            "UWAGA: nie udało się odczytać poprzedniej tożsamości urządzenia (zachowana jako {}).\n\
             Utworzono nową. Znajomi zobaczą Cię jako „nowe urządzenie” i powinni porównać z Tobą kod bezpieczeństwa.",
            broken.display()
        ))),
    }
    let known = KnownDevices::load(&data_dir.join("known_devices.toml"))?;
    if let Some(broken) = known.recovered_from() {
        notices.push(warning(format!(
            "UWAGA: nie udało się odczytać listy zaufanych urządzeń (odłożona jako {}).\n\
             Zaczynamy od pustej listy: wszyscy znajomi pojawią się jako nowe osoby, \
             a wcześniejsze weryfikacje kodów trzeba powtórzyć.",
            broken.display()
        )));
    }
    let trust = Arc::new(Mutex::new(known));
    let mut session = spawn_session(
        rt,
        SessionParams {
            server: account.server,
            fingerprint: account.fingerprint,
            username: account.username,
            password: account.password,
            identity: Arc::new(identity),
            trust: trust.clone(),
        },
    );
    // The session stops when the handle's stop sender drops, so only the channels move out.
    let audio_in = std::mem::replace(&mut session.audio_in, tokio::sync::mpsc::channel(1).1);
    let audio_out = session.audio_out.clone();

    let controls = Arc::new(Controls::default());
    let gains = Arc::new(GainTable::new());
    let activity = Arc::new(Activity::new());
    let engine = AudioEngine::start(
        audio.capture,
        audio.render,
        audio.sensitivity,
        audio_out,
        audio_in,
        controls.clone(),
        gains.clone(),
        activity.clone(),
    );
    Ok((Client { session, engine, trust, controls, gains, activity }, notices))
}

fn warning(text: String) -> Notice {
    Notice { text, warning: true }
}
