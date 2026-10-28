//! Headless voice client: komunikator --server host:port --fingerprint HEX --user NAME

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use client_core::net::session::{spawn_session, SessionEvent, SessionParams};

#[derive(Parser)]
#[command(name = "komunikator", version, about = "Lekki komunikator głosowy")]
struct Cli {
    /// Server address, host:port
    #[arg(long)]
    server: Option<String>,
    /// Server certificate fingerprint (64 hex chars, printed by the server at start)
    #[arg(long)]
    fingerprint: Option<String>,
    #[arg(long)]
    user: Option<String>,
    /// Capture device id (see --list-devices); default communications microphone otherwise
    #[arg(long)]
    capture_device: Option<String>,
    /// Render device id (see --list-devices); default communications output otherwise
    #[arg(long)]
    render_device: Option<String>,
    /// Print audio devices and exit
    #[arg(long)]
    list_devices: bool,
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use client_core::audio::codec::Encoder;
    use client_core::audio::vad::Vad;
    use client_win::pipeline::{capture_loop, render_loop, Controls};
    use client_win::supervisor::Supervised;
    use client_win::wasapi::{self, enter_pro_audio, WasapiCapture, WasapiRender};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    if cli.list_devices {
        for (id, name) in wasapi::list_capture_devices() {
            println!("mikrofon : {name}\n           {id}");
        }
        for (id, name) in wasapi::list_render_devices() {
            println!("głośniki : {name}\n           {id}");
        }
        return Ok(());
    }
    let (Some(server), Some(fp), Some(user)) = (cli.server, cli.fingerprint, cli.user) else {
        anyhow::bail!("wymagane: --server, --fingerprint, --user");
    };
    let fingerprint = protocol::parse_fingerprint(&fp)
        .ok_or_else(|| anyhow::anyhow!("--fingerprint musi mieć 64 znaki hex"))?;
    let password = std::env::var("KOMUNIKATOR_PASSWORD")
        .map_err(|_| anyhow::anyhow!("ustaw hasło w zmiennej KOMUNIKATOR_PASSWORD"))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let mut session = spawn_session(
        rt.handle(),
        SessionParams { server, fingerprint, username: user, password },
    );
    // The session stops when the handle's stop sender drops, so only the channels move out.
    let audio_in = std::mem::replace(&mut session.audio_in, tokio::sync::mpsc::channel(1).1);
    let audio_out = session.audio_out.clone();

    let controls = Arc::new(Controls::default());
    let stop = Arc::new(AtomicBool::new(false));

    let capture = {
        let (controls, stop, device) = (controls.clone(), stop.clone(), cli.capture_device);
        std::thread::Builder::new().name("capture".into()).spawn(move || {
            let _mmcss = enter_pro_audio();
            let src = Supervised::new(move || WasapiCapture::open(device.as_deref()), std::time::Instant::now);
            match Encoder::new() {
                Ok(enc) => capture_loop(src, Vad::new(0.5), enc, audio_out, controls, stop),
                Err(e) => eprintln!("opus: {e}"),
            }
        })?
    };
    let render = {
        let (controls, stop, device) = (controls.clone(), stop.clone(), cli.render_device);
        std::thread::Builder::new().name("render".into()).spawn(move || {
            let _mmcss = enter_pro_audio();
            let sink = Supervised::new(move || WasapiRender::open(device.as_deref()), std::time::Instant::now);
            render_loop(sink, audio_in, controls, stop);
        })?
    };

    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if cmd_tx.send(line.trim().to_lowercase()).is_err() {
                break;
            }
        }
    });

    println!("komendy: m = mikrofon wł./wył., d = wycisz wszystkich, q = wyjście");
    let exit_code = rt.block_on(async {
        loop {
            tokio::select! {
                e = session.events.recv() => match e {
                    Some(SessionEvent::Fatal(reason)) => {
                        println!("błąd: {reason:?}");
                        return 1;
                    }
                    Some(e) => print_event(&e),
                    None => return 1,
                },
                c = cmd_rx.recv() => match c.as_deref() {
                    Some("m") => toggle("mikrofon wyciszony", &controls.muted),
                    Some("d") => toggle("dźwięk wyciszony", &controls.deafened),
                    Some("q") | None => return 0,
                    _ => {}
                },
            }
        }
    });

    stop.store(true, Ordering::Relaxed);
    session.shutdown();
    let _ = capture.join();
    let _ = render.join();
    std::process::exit(exit_code);
}

fn toggle(label: &str, flag: &AtomicBool) {
    let now = !flag.fetch_xor(true, Ordering::Relaxed);
    println!("{label}: {}", if now { "TAK" } else { "nie" });
}

fn print_event(e: &SessionEvent) {
    match e {
        SessionEvent::Connecting => println!("łączenie…"),
        SessionEvent::Connected { you, members } => {
            let names: Vec<_> = members.iter().map(|m| m.name.as_str()).collect();
            println!("połączono (id {you}); w pokoju: {}", if names.is_empty() { "nikt".into() } else { names.join(", ") });
        }
        SessionEvent::MemberJoined(m) => println!("+ {} dołącza", m.name),
        SessionEvent::MemberLeft(id) => println!("- uczestnik {id} wychodzi"),
        SessionEvent::Disconnected { retry_in } => {
            println!("rozłączono, ponowna próba za {:.1} s", retry_in.as_secs_f32())
        }
        SessionEvent::Fatal(r) => println!("błąd: {r:?}"),
    }
}

#[cfg(not(windows))]
fn main() {
    let _ = Cli::parse();
    eprintln!("komunikator działa tylko na Windows");
}
