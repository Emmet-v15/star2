use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::Duration;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use star2_engine::{start_call, CallConfig, Event};

const USAGE: &str = "\
star2-engine - P2P voice call (48 kHz Opus, direct UDP)

Normally launched by `star2`, which supervises and updates it. Running it
directly is fine too - you just don't get auto-update.

USAGE:
    star2-engine [OPTIONS]

OPTIONS:
    --url <URL>        signal server           [default: wss://star.v15.studio/star2]
    --room <TOKEN>     room token to join                      [default: general]
    --create <NAME>    make a new room token and join it (prefer `star2 --create`)
    --name <NAME>      your display name                       [default: this PC's name]
    --token <TOKEN>    shared secret                           [default: baked in]
    --mono             send 1 channel instead of 2
    --bitrate <BPS>    Opus bitrate                            [default: 256000 stereo / 128000 mono]
    --input <SUBSTR>   input device name match                 [default: system default]
    --output <SUBSTR>  output device name match                [default: system default]
    --dev-buf <MS>     device buffer request, 0 = default      [default: 0]
    --stats            print the 1 Hz jitter/loss/rate readout
    --verbose          print the audio/punching/jitter internals
    --list-devices     print audio devices and exit
    -h, --help         this
";

fn main() -> Result<()> {
    let mut cfg = CallConfig::default();
    let mut bitrate: Option<i32> = None;
    let mut stats = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_default();
        match a.as_str() {
            "--url" => cfg.url = val(),
            "--room" => cfg.room_token = val(),
            "--create" => cfg.room_token = star2_proto::new_room_token(&val()),
            "--name" => cfg.name = val(),
            "--token" => cfg.token = val(),
            "--mono" => cfg.stereo = false,
            "--bitrate" => bitrate = val().parse().ok(),
            "--input" => cfg.input = val(),
            "--output" => cfg.output = val(),
            "--dev-buf" => cfg.dev_buf_ms = val().parse().unwrap_or(0),
            "--stats" => stats = true,
            "--verbose" => star2_engine::set_verbose(true),
            "--list-devices" => return list_devices(),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => {
                eprintln!("unknown argument {other:?}\n");
                print!("{USAGE}");
                std::process::exit(2);
            }
        }
    }

    cfg.bitrate = bitrate.unwrap_or(if cfg.stereo { 256_000 } else { 128_000 });
    if cfg.name == "anon" {
        if let Some(h) = pc_name() {
            cfg.name = h;
        }
    }

    println!(
        "[engine] {} joining {} as {:?} ({}, {} kbps)",
        env!("CARGO_PKG_VERSION"),
        cfg.room_token,
        cfg.name,
        if cfg.stereo { "stereo" } else { "mono" },
        cfg.bitrate / 1000
    );

    let (tx, rx) = channel();
    let _call = start_call(cfg, move |e| {
        let _ = tx.send(e);
    })?;

    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::Status(s)) => println!("[engine] {s}"),
            Ok(Event::Direct(_)) => {}
            Ok(Event::Stats { jitter_ms, target_ms, loss_pct, out_ms, rx_pps, play_fps }) => {
                if stats {
                    println!("  jitter {jitter_ms:.1}ms  buf {target_ms:.0}ms  loss {loss_pct:.1}%  out {out_ms:.0}ms  rx {rx_pps}/s  play {play_fps}/s");
                }
            }
            Ok(Event::Ended(why)) => {
                println!("[engine] call ended: {why}");
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn pc_name() -> Option<String> {
    let raw = gethostname::gethostname().to_string_lossy().into_owned();
    let name = raw.trim().trim_end_matches('.');
    let name = name.strip_suffix(".local").unwrap_or(name);
    let name = name.split('.').next().unwrap_or(name).trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    let dflt_in = host.default_input_device().and_then(|d| d.name().ok()).unwrap_or_default();
    let dflt_out = host.default_output_device().and_then(|d| d.name().ok()).unwrap_or_default();
    println!("inputs:");
    for d in host.input_devices()? {
        let n = d.name().unwrap_or_default();
        println!("  {}{}", n, if n == dflt_in { "  (default)" } else { "" });
    }
    println!("outputs:");
    for d in host.output_devices()? {
        let n = d.name().unwrap_or_default();
        println!("  {}{}", n, if n == dflt_out { "  (default)" } else { "" });
    }
    Ok(())
}
