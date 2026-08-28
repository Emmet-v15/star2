use std::f32::consts::PI;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn main() {
    let want = std::env::args().nth(1).unwrap_or_else(|| "stable".into());
    let host = cpal::default_host();
    let device = host
        .output_devices()
        .expect("enumerate outputs")
        .find(|d| d.name().map(|n| n.to_lowercase().contains(&want.to_lowercase())).unwrap_or(false))
        .expect("no output device matching");
    println!("playing pink noise -> {}", device.name().unwrap());

    let cfg = device.default_output_config().expect("default config");
    let sample_rate = cfg.sample_rate().0 as f32;
    let channels = cfg.channels() as usize;

    let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) = (0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let mut phase = 0.0f32;
    let mut on = move || {
        let white = (rand_unit() * 2.0 - 1.0) as f64;
        b0 = 0.99886 * b0 + white * 0.0555179;
        b1 = 0.99332 * b1 + white * 0.0750759;
        b2 = 0.96900 * b2 + white * 0.1538520;
        b3 = 0.86650 * b3 + white * 0.3104856;
        b4 = 0.55000 * b4 + white * 0.5329522;
        b5 = -0.76160 * b5 - white * 0.0168980;
        let pink = (b0 + b1 + b2 + b3 + b4 + b5 + b6 + white * 0.5362) as f32 * 0.08;
        b6 = white * 0.115926;

        phase += 220.0 / sample_rate * PI;
        let slow = 0.15 + 0.05 * phase.sin();
        pink * slow
    };

    let stream = device
        .build_output_stream(
            &cfg.clone().into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let s = on();
                    for c in frame {
                        *c = s;
                    }
                }
            },
            |e| eprintln!("audio: {e}"),
            None,
        )
        .expect("build stream");
    stream.play().expect("play");
    println!("ctrl-c to stop");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn rand_unit() -> f64 {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).ok();
    u32::from_le_bytes(buf) as f64 / u32::MAX as f64
}
