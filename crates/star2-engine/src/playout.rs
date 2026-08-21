use std::collections::HashMap;

use audiopus::coder::Decoder;
use audiopus::{Channels, SampleRate};
use star2_proto::flags;

use crate::*;

pub(crate) fn seq_lt(a: u16, b: u16) -> bool {
    a != b && b.wrapping_sub(a) < 0x8000
}

pub(crate) struct AudioPkt {
    pub(crate) flags: u8,
    pub(crate) data: Vec<u8>,
    pub(crate) red: Option<Vec<u8>>,
    pub(crate) arr: Instant,
}

pub(crate) enum Playout {

    Idle,

    Rendered,

    Concealed,

    Expanded,
}

pub(crate) struct DecState {
    pub(crate) dec: Decoder,
    pub(crate) dec_ch: usize,
    pub(crate) next: Option<u16>,
    pub(crate) started: bool,

    pub(crate) dead: u32,
    pub(crate) jb_sum_us: u64,
    pub(crate) jb_n: u64,
    pub(crate) over: u32,
    pub(crate) contracted: u64,
    pub(crate) dead_recv: u64,

    pub(crate) late_dropped: u64,

    pub(crate) resyncs: u64,

    pub(crate) stalled: u32,

    pub(crate) recovered: u64,
}

impl DecState {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            dec: Decoder::new(SampleRate::Hz48000, Channels::Mono)?,
            dec_ch: 1,
            next: None,
            started: false,
            dead: 0,
            jb_sum_us: 0,
            jb_n: 0,
            over: 0,
            contracted: 0,
            dead_recv: 0,
            late_dropped: 0,
            resyncs: 0,
            stalled: 0,
            recovered: 0,
        })
    }

    pub(crate) fn ensure_ch(&mut self, ch: usize) {
        if self.dec_ch != ch {
            let want = if ch == 2 { Channels::Stereo } else { Channels::Mono };
            if let Ok(d) = Decoder::new(SampleRate::Hz48000, want) {
                self.dec = d;
                self.dec_ch = ch;
            }
        }
    }

    pub(crate) fn render(&mut self, pkt: &AudioPkt, out: &mut [f32], scratch: &mut [i16]) {
        self.render_data(&pkt.data, pkt.flags, out, scratch);
    }

    pub(crate) fn render_data(
        &mut self,
        data: &[u8],
        pkt_flags: u8,
        out: &mut [f32],
        scratch: &mut [i16],
    ) {
        let ch = if pkt_flags & flags::STEREO != 0 { 2 } else { 1 };
        self.ensure_ch(ch);
        match self.dec.decode(Some(data), &mut scratch[..FRAME * ch], false) {
            Ok(dn) => Self::upmix(out, ch, dn, |i| scratch[i] as f32 / 32768.0),
            Err(_) => out.iter_mut().for_each(|x| *x = 0.0),
        }
    }

    pub(crate) fn plc(&mut self, out: &mut [f32], scratch: &mut [i16]) {
        match self.dec.decode(None::<&[u8]>, &mut scratch[..FRAME * self.dec_ch], false) {
            Ok(dn) => Self::upmix(out, self.dec_ch, dn, |i| scratch[i] as f32 / 32768.0),
            Err(_) => out.iter_mut().for_each(|x| *x = 0.0),
        }
    }

    pub(crate) fn upmix(out: &mut [f32], ch: usize, n: usize, get: impl Fn(usize) -> f32) {
        for i in 0..FRAME {
            if i < n {
                if ch == 1 {
                    let s = get(i);
                    out[2 * i] = s;
                    out[2 * i + 1] = s;
                } else {
                    out[2 * i] = get(2 * i);
                    out[2 * i + 1] = get(2 * i + 1);
                }
            } else {
                out[2 * i] = 0.0;
                out[2 * i + 1] = 0.0;
            }
        }
    }

    pub(crate) fn produce(
        &mut self,
        pkts: &mut HashMap<u16, AudioPkt>,
        target: usize,
        out: &mut [f32],
        scratch: &mut [i16],
    ) -> Playout {
        if !self.started {

            if pkts.is_empty() || pkts.len() < target {
                out.iter_mut().for_each(|x| *x = 0.0);
                return Playout::Idle;
            }
            let start = pkts.keys().copied().reduce(|a, b| if seq_lt(a, b) { a } else { b }).unwrap();
            self.next = Some(start);
            self.started = true;
        }
        let mut n = self.next.unwrap();
        let before = pkts.len();
        pkts.retain(|&seq, _| !seq_lt(seq, n));

        self.late_dropped += (before - pkts.len()) as u64;

        if pkts.len() > target + SHED_MARGIN {
            for _ in 0..(pkts.len() - target) {
                pkts.remove(&n);
                n = n.wrapping_add(1);
            }
            self.over = 0;
        } else if pkts.len() > target + 1 {
            self.over += 1;
            if self.over >= CONTRACT_EVERY {
                self.over = 0;
                self.contracted += 1;
                pkts.remove(&n);
                n = n.wrapping_add(1);
            }
        } else {
            self.over = 0;
        }
        let outcome = match pkts.remove(&n) {
            Some(pkt) => {
                self.stalled = 0;
                self.jb_sum_us += pkt.arr.elapsed().as_micros() as u64;
                self.jb_n += 1;
                self.render(&pkt, out, scratch);
                Playout::Rendered
            }
            None => {
                let carrier = pkts
                    .get(&n.wrapping_add(1))
                    .and_then(|p| p.red.as_ref().map(|r| (p.flags, r.clone())));
                match carrier {
                    Some((f, data)) => {
                        self.stalled = 0;
                        self.recovered += 1;
                        self.render_data(&data, f, out, scratch);
                        Playout::Rendered
                    }
                    None if pkts.len() < target && self.stalled < target.max(1) as u32 => {

                        self.stalled += 1;
                        self.plc(out, scratch);
                        return Playout::Expanded;
                    }
                    None => {
                        self.stalled = 0;
                        self.plc(out, scratch);
                        Playout::Concealed
                    }
                }
            }
        };
        self.next = Some(n.wrapping_add(1));
        outcome
    }

    pub(crate) fn resync(&mut self, pkts: &mut HashMap<u16, AudioPkt>) {
        self.started = false;
        self.next = None;
        self.stalled = 0;
        self.resyncs += 1;
        pkts.clear();
    }
}

pub(crate) struct DelayEstimator {
    pub(crate) bins: [f32; JB_BINS],
    pub(crate) total: f32,
    pub(crate) since_decay: u32,

    pub(crate) mean_abs: f64,

    pub(crate) spike_ms: f64,
}

impl Default for DelayEstimator {
    fn default() -> Self {

        Self { bins: [0.0; JB_BINS], total: 0.0, since_decay: 0, mean_abs: 0.0, spike_ms: 0.0 }
    }
}

impl DelayEstimator {

    pub(crate) fn observe(&mut self, d: f64) {

        if self.mean_abs > 0.5 && d > SPIKE_MULT * self.mean_abs {
            self.spike_ms = self.spike_ms.max(d.min(SPIKE_MAX_MS));
        }
        self.mean_abs += (d - self.mean_abs) / 32.0;
        let b = ((d / JB_BIN_MS) as usize).min(JB_BINS - 1);
        self.bins[b] += 1.0;
        self.total += 1.0;
        self.since_decay += 1;
        if self.since_decay >= JB_DECAY_EVERY {
            for x in self.bins.iter_mut() {
                *x *= 0.5;
            }
            self.total *= 0.5;
            self.since_decay = 0;
        }
        self.spike_ms *= SPIKE_DECAY;
        if self.spike_ms < 1.0 {
            self.spike_ms = 0.0;
        }
    }

    pub(crate) fn percentile(&self, p: f64) -> f64 {
        if self.total < 1.0 {
            return 0.0;
        }
        let want = p * self.total as f64;
        let mut acc = 0.0f64;
        for (i, &c) in self.bins.iter().enumerate() {
            acc += c as f64;
            if acc >= want {
                return (i as f64 + 0.5) * JB_BIN_MS;
            }
        }
        JB_BINS as f64 * JB_BIN_MS
    }

    pub(crate) fn target_ms(&self, p: f64, k: f64, margin: f64) -> f64 {
        (k * self.percentile(p) + margin).max(self.spike_ms)
    }
}

#[derive(Default)]
pub(crate) struct SenderBuf {
    pub(crate) pkts: HashMap<u16, AudioPkt>,
    pub(crate) est: DelayEstimator,
    pub(crate) last_arr_ms: Option<f64>,
    pub(crate) last_ts: Option<u32>,

    pub(crate) recv_count: u64,
    pub(crate) played: u64,

    pub(crate) concealed: u64,

    pub(crate) expanded: u64,

    pub(crate) target_frames: usize,
}

impl SenderBuf {

    pub(crate) fn insert_capped(&mut self, seq: u16, pkt: AudioPkt) {
        if self.pkts.len() >= 256 {
            if let Some(&oldest) = self.pkts.keys().reduce(|a, b| if seq_lt(*a, *b) { a } else { b }) {
                self.pkts.remove(&oldest);
            }
        }
        self.pkts.insert(seq, pkt);
    }
}

#[cfg(test)]
mod audio_fidelity {

    use super::*;
    use audiopus::coder::Encoder;
    use audiopus::{Application, Bitrate};

    fn tone_energy(samples: &[f32], step: usize, offset: usize, freq: f32) -> f32 {
        let x: Vec<f32> = samples.iter().skip(offset).step_by(step).copied().collect();
        let n = x.len() as f32;
        let k = (0.5 + n * freq / SR as f32).floor();
        let w = 2.0 * std::f32::consts::PI * k / n;
        let (cw, sw) = (w.cos(), w.sin());
        let coeff = 2.0 * cw;
        let (mut q1, mut q2) = (0.0f32, 0.0f32);
        for &s in &x {
            let q0 = coeff * q1 - q2 + s;
            q2 = q1;
            q1 = q0;
        }
        let (re, im) = (q1 - q2 * cw, q2 * sw);
        (re * re + im * im).sqrt() / n
    }

    fn rms(samples: &[f32], step: usize, offset: usize) -> f32 {
        let x: Vec<f32> = samples.iter().skip(offset).step_by(step).copied().collect();
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn through_pipeline(
        ch: usize,
        frames: usize,
        gen: impl Fn(usize, usize) -> f32,
        keep: impl Fn(usize) -> bool,
    ) -> (Vec<f32>, usize) {
        through_pipeline_red(ch, frames, gen, keep, false).0
    }

    fn through_pipeline_red(
        ch: usize,
        frames: usize,
        gen: impl Fn(usize, usize) -> f32,
        keep: impl Fn(usize) -> bool,
        red: bool,
    ) -> ((Vec<f32>, usize), u64) {
        let channels = if ch == 2 { Channels::Stereo } else { Channels::Mono };
        let mut enc = Encoder::new(SampleRate::Hz48000, channels, Application::LowDelay).unwrap();
        enc.set_bitrate(Bitrate::BitsPerSecond(128_000)).unwrap();

        let mut sb = SenderBuf::default();
        let flags = if ch == 2 { star2_proto::flags::STEREO } else { 0 };
        let mut pcm = vec![0i16; FRAME * ch];
        let mut payload = vec![0u8; 4096];

        let mut dec = DecState::new().unwrap();
        let mut out = Vec::with_capacity(frames * STEREO_FRAME);
        let mut frame = vec![0.0f32; STEREO_FRAME];
        let mut scratch = vec![0i16; FRAME * 2];
        let mut concealed = 0;
        let mut prev: Option<Vec<u8>> = None;

        for f in 0..frames {
            for i in 0..FRAME {
                for c in 0..ch {
                    let s = gen(f * FRAME + i, c).clamp(-1.0, 1.0);
                    pcm[i * ch + c] = (s * 32767.0) as i16;
                }
            }
            let n = enc.encode(&pcm, &mut payload).unwrap();
            let cur = payload[..n].to_vec();
            if keep(f) {
                let carried = if red { prev.clone() } else { None };
                sb.insert_capped(
                    f as u16,
                    AudioPkt { flags, data: cur.clone(), red: carried, arr: Instant::now() },
                );
            }
            prev = Some(cur);

            match dec.produce(&mut sb.pkts, 1, &mut frame, &mut scratch) {
                Playout::Rendered => out.extend_from_slice(&frame),
                Playout::Concealed | Playout::Expanded => {
                    concealed += 1;
                    out.extend_from_slice(&frame);
                }
                Playout::Idle => {}
            }
        }
        ((out, concealed), dec.recovered)
    }

    fn all(_: usize) -> bool {
        true
    }

    #[test]
    fn mono_tone_survives_the_pipeline() {
        let tone = |i: usize, _c: usize| {
            (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.5
        };
        let (out, _) = through_pipeline(1, 60, tone, all);
        assert!(!out.is_empty(), "pipeline produced nothing at all");

        let steady = &out[20 * STEREO_FRAME..];

        let level = rms(steady, 2, 0);
        assert!(level > 0.2, "output far too quiet (rms {level}) - silence or a scaling bug");
        assert!(level < 0.6, "output far too loud (rms {level}) - a scaling bug");

        let at_1k = tone_energy(steady, 2, 0, 1000.0);
        let at_3k = tone_energy(steady, 2, 0, 3000.0);
        assert!(
            at_1k > at_3k * 10.0,
            "1 kHz tone did not survive: 1k={at_1k:.5} vs 3k={at_3k:.5} - output is noise, not the input"
        );
    }

    #[test]
    fn mono_upmixes_to_both_channels() {
        let tone = |i: usize, _c: usize| {
            (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.5
        };
        let (out, _) = through_pipeline(1, 40, tone, all);
        let steady = &out[20 * STEREO_FRAME..];
        for (i, pair) in steady.chunks(2).enumerate() {
            assert_eq!(pair[0], pair[1], "L/R differ at frame {i} - mono did not upmix");
        }
    }

    #[test]
    fn stereo_keeps_channels_separate() {
        let split = |i: usize, c: usize| {
            if c == 0 {
                (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.5
            } else {
                0.0
            }
        };
        let (out, _) = through_pipeline(2, 60, split, all);
        let steady = &out[20 * STEREO_FRAME..];

        let l = tone_energy(steady, 2, 0, 1000.0);
        let r = tone_energy(steady, 2, 1, 1000.0);
        assert!(l > 0.05, "left channel lost the tone (l={l:.5})");

        assert!(l > r * 5.0, "channels bled together: l={l:.5} r={r:.5}");
    }

    #[test]
    fn silence_stays_silent() {
        let (out, _) = through_pipeline(1, 40, |_, _| 0.0, all);
        let steady = &out[20 * STEREO_FRAME..];
        let level = rms(steady, 2, 0);
        assert!(level < 0.01, "silence produced output (rms {level})");
    }

    #[test]
    fn tone_survives_lost_packets() {
        let tone = |i: usize, _c: usize| {
            (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.5
        };

        let (out, concealed) = through_pipeline(1, 80, tone, |f| f % 10 != 3);
        assert!(concealed >= 5, "expected the dropped packets to conceal, got {concealed}");

        let steady = &out[20 * STEREO_FRAME..];
        let at_1k = tone_energy(steady, 2, 0, 1000.0);
        let at_3k = tone_energy(steady, 2, 0, 3000.0);
        assert!(
            at_1k > at_3k * 5.0,
            "tone lost after concealment: 1k={at_1k:.5} 3k={at_3k:.5}"
        );

        let level = rms(steady, 2, 0);
        assert!(level > 0.15, "concealment left the audio too quiet (rms {level})");
    }

    #[test]
    fn redundancy_recovers_lost_packets() {
        let tone = |i: usize, _c: usize| {
            (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.5
        };
        let drop = |f: usize| f % 10 != 3;

        let ((_, plain_concealed), plain_rec) = through_pipeline_red(1, 80, tone, drop, false);
        let ((out, red_concealed), red_rec) = through_pipeline_red(1, 80, tone, drop, true);

        assert_eq!(plain_rec, 0, "no redundancy was sent, yet something was recovered");
        assert!(red_rec >= 5, "redundancy recovered only {red_rec} of the dropped frames");
        assert!(
            red_concealed < plain_concealed,
            "redundancy did not reduce concealment: {red_concealed} vs {plain_concealed}"
        );

        let steady = &out[20 * STEREO_FRAME..];
        let at_1k = tone_energy(steady, 2, 0, 1000.0);
        let at_3k = tone_energy(steady, 2, 0, 3000.0);
        assert!(at_1k > at_3k * 10.0, "recovered audio is noise: 1k={at_1k:.5} 3k={at_3k:.5}");
    }

    #[test]
    fn redundancy_is_inert_when_nothing_is_lost() {
        let tone = |i: usize, _c: usize| {
            (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.5
        };
        let ((out, concealed), recovered) = through_pipeline_red(1, 60, tone, all, true);
        assert_eq!(recovered, 0, "recovered {recovered} frames on a lossless link");
        assert_eq!(concealed, 0, "concealed {concealed} frames on a lossless link");

        let level = rms(&out[20 * STEREO_FRAME..], 2, 0);
        assert!(level > 0.2 && level < 0.6, "carrying redundancy changed the audio (rms {level})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_wraps() {
        assert!(seq_lt(0xFFFE, 0x0001));
        assert!(!seq_lt(0x0001, 0xFFFE));
        assert!(!seq_lt(5, 5));
    }

    #[test]
    fn percentile_tracks_distribution() {
        let mut e = DelayEstimator::default();
        for _ in 0..100 {
            e.observe(2.0);
        }

        assert!((e.percentile(0.97) - 2.0).abs() < 0.01);
    }

    #[test]
    fn spike_raises_target_then_decays() {
        let mut e = DelayEstimator::default();
        for _ in 0..200 {
            e.observe(1.0);
        }
        let calm = e.target_ms(JB_PCTILE, JITTER_K, JITTER_MARGIN_MS);
        e.observe(120.0);
        assert!(e.target_ms(JB_PCTILE, JITTER_K, JITTER_MARGIN_MS) > calm, "spike must raise target");
        for _ in 0..2000 {
            e.observe(1.0);
        }
        assert_eq!(e.spike_ms, 0.0, "spike bump must decay away");
    }

    #[test]
    fn evicts_oldest_when_full() {
        let mut sb = SenderBuf::default();
        for seq in 0..300u16 {
            sb.insert_capped(seq, AudioPkt { flags: 0, data: vec![], red: None, arr: Instant::now() });
        }
        assert!(sb.pkts.len() <= 256);

        assert!(sb.pkts.contains_key(&299));
        assert!(!sb.pkts.contains_key(&0));
    }
}
