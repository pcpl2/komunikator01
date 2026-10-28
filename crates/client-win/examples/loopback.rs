//! Manual check: microphone straight to speakers for 10 s.
//! With --silent the speakers get silence and only capture levels are printed;
//! --render-only writes 2 s of silence to check output pacing.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use client_core::audio::frame::FRAME_SAMPLES;
    use client_win::wasapi::{self, enter_pro_audio, WasapiCapture, WasapiRender};
    use client_win::{FrameSink, FrameSource};

    let silent = std::env::args().any(|a| a == "--silent");
    for (id, name) in wasapi::list_capture_devices() {
        println!("in : {name}  [{id}]");
    }
    for (id, name) in wasapi::list_render_devices() {
        println!("out: {name}  [{id}]");
    }
    let _mmcss = enter_pro_audio();
    if std::env::args().any(|a| a == "--render-only") {
        // Machines without a microphone: check that output paces at real time.
        let mut spk = WasapiRender::open(None).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let start = std::time::Instant::now();
        for _ in 0..100 {
            spk.write_frame(&[0f32; FRAME_SAMPLES]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        println!("render-only: 100 frames in {:.2} s", start.elapsed().as_secs_f32());
        return Ok(());
    }
    let mut mic = WasapiCapture::open(None).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut spk = WasapiRender::open(None).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut frame = [0f32; FRAME_SAMPLES];
    let silence = [0f32; FRAME_SAMPLES];
    let start = std::time::Instant::now();
    let mut frames = 0u32;
    let mut peak = 0f32;
    while start.elapsed().as_secs() < 10 {
        mic.read_frame(&mut frame).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        peak = frame.iter().fold(peak, |p, x| p.max(x.abs()));
        spk.write_frame(if silent { &silence } else { &frame }).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        frames += 1;
        if frames % 50 == 0 {
            println!("{:>4} frames, peak {peak:.3}", frames);
            peak = 0.0;
        }
    }
    println!("done: {frames} frames in {:.1} s", start.elapsed().as_secs_f32());
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
