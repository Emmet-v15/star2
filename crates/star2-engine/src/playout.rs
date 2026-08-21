//! Receive-side audio: jitter estimation, per-sender buffering, decode/PLC playout.
//!
//! Ported from star v1, trimmed to what a 1:1 MVP needs (telemetry accumulators and
//! the multi-sender mixing dropped). The algorithms are unchanged - they are the part
//! that took the longest to get right.

use std::collections::HashMap;

use audiopus::coder::Decoder;
use audiopus::{Channels, SampleRate};
use star2_proto::flags;

use crate::*;

/// Sequence comparison over a wrapping u16 space.
pub(crate) fn seq_lt(a: u16, b: u16) -> bool {
    a != b && b.wrapping_sub(a) < 0x8000
}

/// One received audio packet: its media flags (channel count) and payload.
pub(crate) struct AudioPkt {
    pub(crate) flags: u8,
    pub(crate) data: Vec<u8>,
}

/// What a single playout cycle produced.
pub(crate) enum Playout {
    /// Still pre-buffering (below target) - not counted as loss.
    Idle,
    /// A real packet was decoded and rendered.
    Rendered,
    /// No packet for this slot - filled by packet-loss concealment (loss/late).
    Concealed,
}

/// Decode state: an Opus decoder (lazily resized to the sender's channel count) plus
/// jitter-buffer sequencing. Produces an interleaved **stereo** frame.
pub(crate) struct DecState {
    pub(crate) dec: Decoder,
    pub(crate) dec_ch: usize,
    pub(crate) next: Option<u16>,
    pub(crate) started: bool,
    /// Consecutive conceal cycles, and `recv_count` at the last render - the desync
    /// watchdog. If we haven't rendered for a while YET packets kept arriving, the seq
    /// tracker has desynced (usually `next` ran ahead, so `retain` drops every arrival
    /// and the buffer stays empty) -> force a re-lock.
    pub(crate) dead: u32,
    pub(crate) dead_recv: u64,
}

impl DecState {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            dec: Decoder::new(SampleRate::Hz48000, Channels::Mono)?,
            dec_ch: 1,
            next: None,
            started: false,
            dead: 0,
            dead_recv: 0,
        })
    }

    /// Swap the decoder if the sender changed channel count (mono <-> stereo).
    pub(crate) fn ensure_ch(&mut self, ch: usize) {
        if self.dec_ch != ch {
            let want = if ch == 2 { Channels::Stereo } else { Channels::Mono };
            if let Ok(d) = Decoder::new(SampleRate::Hz48000, want) {
                self.dec = d;
                self.dec_ch = ch;
            }
        }
    }

    /// Decode one packet into `out` (interleaved stereo, STEREO_FRAME samples).
    pub(crate) fn render(&mut self, pkt: &AudioPkt, out: &mut [f32], scratch: &mut [i16]) {
        let ch = if pkt.flags & flags::STEREO != 0 { 2 } else { 1 };
        self.ensure_ch(ch);
        match self.dec.decode(Some(&pkt.data), &mut scratch[..FRAME * ch], false) {
            Ok(dn) => Self::upmix(out, ch, dn, |i| scratch[i] as f32 / 32768.0),
            Err(_) => out.iter_mut().for_each(|x| *x = 0.0),
        }
    }

    /// Packet-loss concealment: let Opus interpolate the missing frame.
    pub(crate) fn plc(&mut self, out: &mut [f32], scratch: &mut [i16]) {
        match self.dec.decode(None::<&[u8]>, &mut scratch[..FRAME * self.dec_ch], false) {
            Ok(dn) => Self::upmix(out, self.dec_ch, dn, |i| scratch[i] as f32 / 32768.0),
            Err(_) => out.iter_mut().for_each(|x| *x = 0.0),
        }
    }

    /// Write `n` frames of `ch`-channel source samples into interleaved stereo `out`
    /// (mono duplicates to L/R), zero-filling past `n`.
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

    /// Emit one frame: decode the next in sequence, or conceal if it hasn't arrived.
    pub(crate) fn produce(
        &mut self,
        pkts: &mut HashMap<u16, AudioPkt>,
        target: usize,
        out: &mut [f32],
        scratch: &mut [i16],
    ) -> Playout {
        if !self.started {
            // Latch on the first packet even at target 0: we can't start a sequence
            // from an empty map.
            if pkts.is_empty() || pkts.len() < target {
                out.iter_mut().for_each(|x| *x = 0.0);
                return Playout::Idle;
            }
            let start = pkts.keys().copied().reduce(|a, b| if seq_lt(a, b) { a } else { b }).unwrap();
            self.next = Some(start);
            self.started = true;
        }
        let mut n = self.next.unwrap();
        pkts.retain(|&seq, _| !seq_lt(seq, n));
        // Far over target: drop ahead so we catch up instead of running a permanent
        // surplus of latency.
        if pkts.len() > target + SHED_MARGIN {
            for _ in 0..(pkts.len() - target) {
                pkts.remove(&n);
                n = n.wrapping_add(1);
            }
        }
        let outcome = match pkts.remove(&n) {
            Some(pkt) => {
                self.render(&pkt, out, scratch);
                Playout::Rendered
            }
            None => {
                self.plc(out, scratch);
                Playout::Concealed
            }
        };
        self.next = Some(n.wrapping_add(1));
        outcome
    }

    /// Force a re-lock to the buffer's freshest packet on the next `produce` - the
    /// desync-watchdog recovery. Clears the buffer so `retain` can't keep dropping
    /// fresh arrivals when `next` has run ahead of the stream.
    pub(crate) fn resync(&mut self, pkts: &mut HashMap<u16, AudioPkt>) {
        self.started = false;
        self.next = None;
        pkts.clear();
    }
}

/// Delay/jitter estimator: an exponentially-windowed histogram of inter-arrival jitter,
/// from which we read a high percentile as the buffer target, plus a fast-attack /
/// slow-decay spike detector. O(1) per packet.
pub(crate) struct DelayEstimator {
    pub(crate) bins: [f32; JB_BINS],
    pub(crate) total: f32,
    pub(crate) since_decay: u32,
    /// EWMA of |jitter| - the spike-detection baseline.
    pub(crate) mean_abs: f64,
    /// Transient target bump (decays back down).
    pub(crate) spike_ms: f64,
}

impl Default for DelayEstimator {
    fn default() -> Self {
        // (arrays > 32 don't derive Default, so hand-roll it)
        Self { bins: [0.0; JB_BINS], total: 0.0, since_decay: 0, mean_abs: 0.0, spike_ms: 0.0 }
    }
}

impl DelayEstimator {
    /// Observe one inter-arrival jitter magnitude `d` (ms).
    pub(crate) fn observe(&mut self, d: f64) {
        // Spike = a sudden jump well above the recent mean deviation. Fast attack: the
        // bump jumps immediately and only relaxes via SPIKE_DECAY below.
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

    /// The `p`-percentile (0..1) of the observed jitter distribution, in ms.
    pub(crate) fn percentile(&self, p: f64) -> f64 {
        if self.total < 1.0 {
            return 0.0;
        }
        let want = p * self.total as f64;
        let mut acc = 0.0f64;
        for (i, &c) in self.bins.iter().enumerate() {
            acc += c as f64;
            if acc >= want {
                return (i as f64 + 0.5) * JB_BIN_MS; // bin center
            }
        }
        JB_BINS as f64 * JB_BIN_MS
    }

    /// Target buffer depth (ms), raised transiently by any active spike bump.
    /// The caller clamps to [floor, MAX] frames.
    pub(crate) fn target_ms(&self, p: f64, k: f64, margin: f64) -> f64 {
        (k * self.percentile(p) + margin).max(self.spike_ms)
    }
}

/// Per-sender receive state.
#[derive(Default)]
pub(crate) struct SenderBuf {
    pub(crate) pkts: HashMap<u16, AudioPkt>,
    pub(crate) est: DelayEstimator,
    pub(crate) last_arr_ms: Option<f64>,
    pub(crate) last_ts: Option<u32>,
    /// Cumulative audio packets received (drives the desync watchdog).
    pub(crate) recv_count: u64,
    pub(crate) played: u64,
    /// Of those, how many were concealment (PLC) frames - loss/late.
    pub(crate) concealed: u64,
    /// Current adaptive jitter-buffer target depth, in frames.
    pub(crate) target_frames: usize,
}

impl SenderBuf {
    /// Insert a packet, evicting the OLDEST (lowest seq) when full. Dropping *new*
    /// packets instead would leave a stale buffer while the sender advances - the
    /// desync that wedges playout at 100% loss.
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
mod tests {
    use super::*;

    #[test]
    fn seq_wraps() {
        assert!(seq_lt(0xFFFE, 0x0001)); // across the wrap point
        assert!(!seq_lt(0x0001, 0xFFFE));
        assert!(!seq_lt(5, 5));
    }

    #[test]
    fn percentile_tracks_distribution() {
        let mut e = DelayEstimator::default();
        for _ in 0..100 {
            e.observe(2.0);
        }
        // All mass in the 0-4ms bin, so any percentile lands at that bin's center.
        assert!((e.percentile(0.97) - 2.0).abs() < 0.01);
    }

    #[test]
    fn spike_raises_target_then_decays() {
        let mut e = DelayEstimator::default();
        for _ in 0..200 {
            e.observe(1.0);
        }
        let calm = e.target_ms(JB_PCTILE, JITTER_K, JITTER_MARGIN_MS);
        e.observe(120.0); // a big late arrival
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
            sb.insert_capped(seq, AudioPkt { flags: 0, data: vec![] });
        }
        assert!(sb.pkts.len() <= 256);
        // The freshest packet survived; the oldest did not.
        assert!(sb.pkts.contains_key(&299));
        assert!(!sb.pkts.contains_key(&0));
    }
}
