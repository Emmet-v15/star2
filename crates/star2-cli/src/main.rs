//! star2 - barebones P2P voice call. The CLI is the whole UI.

use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::Duration;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use star2_engine::{start_call, CallConfig, Event};

const USAGE: &str = "\
star2 - P2P voice call (48 kHz Opus, direct UDP)

USAGE:
    star2 [OPTIONS]

OPTIONS:
    --url <URL>        signal server, e.g. ws://empire:9101   [default: ws://127.0.0.1:9101]
    --room <NAME>      room to join                            [default: general]
    --name <NAME>      your display name                       [default: hostname]
    --token <TOKEN>    shared secret                           [default: star2-dev]
    --stereo           send 2 channels instead of 1
    --bitrate <BPS>    Opus bitrate                            [default: 128000 mono / 256000 stereo]
    --input <SUBSTR>   input device name match                 [default: system default]
    --output <SUBSTR>  output device name match                [default: system default]
    --dev-buf <MS>     device buffer request, 0 = default      [default: 0]
    --list-devices     print audio devices and exit
    -h, --help         this
";

fn main() -> Result<()> {
    let mut cfg = CallConfig::default();
    let mut bitrate: Option<i32> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_default();
        match a.as_str() {
            "--url" => cfg.url = val(),
            "--room" => cfg.room = val(),
            "--name" => cfg.name = val(),
            "--token" => cfg.token = val(),
            "--stereo" => cfg.stereo = true,
            "--bitrate" => bitrate = val().parse().ok(),
            "--input" => cfg.input = val(),
            "--output" => cfg.output = val(),
            "--dev-buf" => cfg.dev_buf_ms = val().parse().unwrap_or(0),
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
    // Stereo carries twice the signal, so give it twice the bits unless told otherwise.
    cfg.bitrate = bitrate.unwrap_or(if cfg.stereo { 256_000 } else { 128_000 });
    if cfg.name == "anon" {
        if let Ok(h) = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")) {
            cfg.name = h;
        }
    }

    println!(
        "star2: {} -> room {:?} as {:?} ({}, {} kbps)",
        cfg.url,
        cfg.room,
        cfg.name,
        if cfg.stereo { "stereo" } else { "mono" },
        cfg.bitrate / 1000
    );

    // The engine calls us from its own threads; funnel events onto this one.
    let (tx, rx) = channel();
    let _call = start_call(cfg, move |e| {
        let _ = tx.send(e);
    })?;

    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::Status(s)) => println!("  {s}"),
            Ok(Event::Direct(addr)) => println!("  direct path up: {addr}"),
            Ok(Event::Stats { jitter_ms, target_ms, loss_pct, out_ms }) => println!(
                "  jitter {jitter_ms:.1}ms  buf {target_ms:.0}ms  loss {loss_pct:.1}%  out {out_ms:.0}ms"
            ),
            Ok(Event::Ended(why)) => {
                println!("call ended: {why}");
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
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
