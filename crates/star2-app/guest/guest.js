// star2 browser guest.
//
// Signaling rides the same rendezvous WebSocket as the native client, and the
// media rides an unreliable, unordered WebRTC data channel carrying exactly
// the star-proto datagrams a native peer would put on UDP:
//
//   byte 0      version (1)
//   byte 1      flags (0 - mono, no RED)
//   bytes 2-6   session id, LE u32
//   bytes 6-8   sequence, LE u16
//   bytes 8-12  timestamp, LE u32, in 48 kHz samples
//   rest        one 5 ms Opus frame (WebCodecs AudioEncoder)
//
// The native side answers the SDP offer with str0m (ICE-lite) and feeds the
// channel data into its ordinary jitter buffer and playout.

"use strict";

const PROTO_VERSION = 1;
const FRAME_SAMPLES = 240; // 5 ms at 48 kHz
const SAMPLE_RATE = 48000;

const $ = (id) => document.getElementById(id);
const params = new URLSearchParams(location.hash.slice(1));
const RENDEZVOUS = params.get("rv") ?? (location.protocol === "https:" ? "wss://star.v15.studio/star2" : "ws://localhost:9101");
const TOKEN = params.get("token") ?? "star2-dev";

function setStatus(text, cls) {
  $("status").innerHTML = `<span class="dot ${cls ?? ""}"></span>${text}`;
  console.log("[guest]", text);
}

let ws = null;
let session = 0;
let peer = 0;
let pc = null;
let dc = null;
let micStream = null;
let audioCtx = null;
let encoder = null;
let decoder = null;
let micPort = null;
let seq = 0;
let ts = 0;
let tsUs = 0;
let rxTsUs = 0;

const send = (msg) => ws.readyState === 1 && ws.send(JSON.stringify(msg));

function connect() {
  setStatus("connecting to rendezvous");
  ws = new WebSocket(RENDEZVOUS);
  ws.onopen = () =>
    send({
      t: "Hello",
      name: params.get("name") ?? "guest-web",
      ver: PROTO_VERSION,
      token: TOKEN,
      build: "guest-web",
    });
  ws.onmessage = (e) => {
    let m;
    try {
      m = JSON.parse(e.data);
    } catch {
      return;
    }
    handleServer(m);
  };
  ws.onclose = () => {
    if (pc) hangup("rendezvous closed");
    else setStatus("rendezvous unreachable", "bad");
  };
}

function handleServer(m) {
  (window.__log = window.__log ?? []).push(`recv ${m.t}` + (m.t === "Error" ? `: ${m.msg}` : ""));
  switch (m.t) {
    case "Welcome":
      session = m.session;
      setStatus(`welcome, session ${session}`);
      send({ t: "Join", room: params.get("room") ?? $("room").value.trim() });
      break;
    case "Room":
      otherPeer(m.members);
      break;
    case "Joined":
      otherPeer([{ session: m.session }, { session }]);
      break;
    case "SdpAnswer":
      if (pc && m.from === peer) {
        pc.setRemoteDescription({ type: "answer", sdp: m.sdp }).catch((e) => {
          setStatus(`answer refused: ${e.message}`, "bad");
        });
      }
      break;
    case "Left":
      if (m.session === peer) hangup("peer left");
      break;
    case "Error":
      setStatus(`server: ${m.msg}`, "bad");
      break;
  }
}

function otherPeer(members) {
  const other = members.find((x) => x.session !== session);
  if (other && !pc) {
    peer = other.session;
    invite(peer);
  }
}

async function invite(to) {
  setStatus("negotiating with native peer");
  try {
    micStream = await navigator.mediaDevices.getUserMedia({
      audio: {
        echoCancellation: false,
        noiseSuppression: false,
        autoGainControl: false,
      },
    });
  } catch (e) {
    setStatus(`microphone refused: ${e.message}`, "bad");
    return;
  }

  pc = new RTCPeerConnection({
    iceServers: [{ urls: ["stun:stun.l.google.com:19302", "stun:stun1.l.google.com:19302"] }],
  });
  dc = pc.createDataChannel("media", { ordered: false, maxRetransmits: 0 });
  dc.binaryType = "arraybuffer";
  dc.onopen = () => {
    (window.__log = window.__log ?? []).push("dc open");
    startAudio().catch((e) => setStatus(`audio: ${e.message}`, "bad"));
  };
  dc.onclose = () => {
    (window.__log = window.__log ?? []).push("dc close");
    hangup("channel closed");
  };
  pc.oniceconnectionstatechange = () =>
    (window.__log = window.__log ?? []).push(`ice: ${pc.iceConnectionState}`);
  pc.onconnectionstatechange = () => {
    (window.__log = window.__log ?? []).push(`pc: ${pc.connectionState}`);
    setStatus(`peer: ${pc.connectionState}`);
    if (pc.connectionState === "failed") hangup("connection failed");
  };

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  await gathered(pc);
  send({ t: "SdpOffer", to, sdp: pc.localDescription.sdp });
  setStatus("offer sent, waiting for answer");
}

function gathered(pc) {
  if (pc.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((res) => {
    const check = () => pc.iceGatheringState === "complete" && res();
    pc.addEventListener("icegatheringstatechange", check);
    setTimeout(res, 2000); // never wait forever on a stalled gather
  });
}

async function startAudio() {
  audioCtx = new AudioContext({ sampleRate: SAMPLE_RATE });
  await audioCtx.audioWorklet.addModule(workletUrl());

  await audioCtx.resume();
  const src = audioCtx.createMediaStreamSource(micStream);
  const capture = new AudioWorkletNode(audioCtx, "capture-worklet");
  capture.port.onmessage = (e) => enqueueForEncoder(e.data);
  src.connect(capture); // no connection to destination: never monitor the mic

  const play = new AudioWorkletNode(audioCtx, "play-worklet");
  micPort = play.port;
  play.connect(audioCtx.destination);

  encoder = new AudioEncoder({
    output: (chunk, meta) => {
      const payload = new Uint8Array(chunk.byteLength);
      chunk.copyTo(payload);
      if (dc && dc.readyState === "open") dc.send(frame(payload));
    },
    error: (e) => setStatus(`encoder: ${e.message}`, "bad"),
  });
  encoder.configure({
    codec: "opus",
    sampleRate: SAMPLE_RATE,
    numberOfChannels: 1,
    bitrate: 128000,
    opus: { frameDuration: 5000 },
  });

  decoder = new AudioDecoder({
    output: (audio) => {
      const f32 = new Float32Array(audio.numberOfFrames);
      audio.copyTo(f32, { planeIndex: 0, format: "f32" });
      micPort.postMessage(f32, [f32.buffer]);
      audio.close();
    },
    error: (e) => setStatus(`decoder: ${e.message}`, "bad"),
  });
  decoder.configure({ codec: "opus", sampleRate: SAMPLE_RATE, numberOfChannels: 1 });

  setStatus("live - media over data channel", "live");
}

function workletUrl() {
  const src = `
    registerProcessor("capture-worklet", class extends AudioWorkletProcessor {
      constructor() { super(); this.buf = new Float32Array(${FRAME_SAMPLES}); this.n = 0; }
      process(inputs) {
        const ch = inputs[0][0];
        if (!ch) return true;
        for (let i = 0; i < ch.length; i++) {
          this.buf[this.n++] = ch[i];
          if (this.n === ${FRAME_SAMPLES}) {
            this.port.postMessage(this.buf.slice(0));
            this.n = 0;
          }
        }
        return true;
      }
    });
    registerProcessor("play-worklet", class extends AudioWorkletProcessor {
      constructor() { super(); this.ring = new Float32Array(8192); this.r = 0; this.w = 0; this.filled = 0;
        this.port.onmessage = (e) => {
          const d = e.data;
          for (let i = 0; i < d.length; i++) {
            this.ring[this.w] = d[i];
            this.w = (this.w + 1) % this.ring.length;
            this.filled = Math.min(this.filled + 1, this.ring.length);
          }
        };
      }
      process(_, outputs) {
        const out = outputs[0][0];
        // wait for one quantum of buffered audio before starting, then run open loop
        if (this.filled < 256 && this.started !== true) { out.fill(0); return true; }
        this.started = true;
        for (let i = 0; i < out.length; i++) {
          if (this.filled > 0) { out[i] = this.ring[this.r]; this.r = (this.r + 1) % this.ring.length; this.filled--; }
          else out[i] = 0;
        }
        return true;
      }
    });
  `;
  return URL.createObjectURL(new Blob([src], { type: "application/javascript" }));
}

// Wrap the Opus payload in a star-proto media header, exactly the bytes the
// native peer would have put in a datagram (see star-proto::MediaHeader).
function frame(payload) {
  seq = (seq + 1) & 0xffff;
  ts = (ts + FRAME_SAMPLES) >>> 0;
  const out = new Uint8Array(12 + payload.length);
  const dv = new DataView(out.buffer);
  dv.setUint8(0, PROTO_VERSION);
  dv.setUint8(1, 0);
  dv.setUint32(2, session, true);
  dv.setUint16(6, seq, true);
  dv.setUint32(8, ts, true);
  out.set(payload, 12);
  return out.buffer;
}

function onChannelData(buf) {
  if (buf.byteLength < 12) return;
  const dv = new DataView(buf);
  const version = dv.getUint8(0);
  if (version !== PROTO_VERSION) return;
  const payload = new Uint8Array(buf, 12);
  rxTsUs += 5000;
  decoder.decode(
    new EncodedAudioChunk({ type: "delta", timestamp: rxTsUs, data: payload })
  );
}

function enqueueForEncoder(f32) {
  if (!encoder) return;
  tsUs += 5000;
  encoder.encode(
    new AudioData({
      format: "f32",
      sampleRate: SAMPLE_RATE,
      numberOfFrames: f32.length,
      numberOfChannels: 1,
      timestamp: tsUs,
      data: f32,
    })
  );
}

function hangup(why) {
  if (dc) try { dc.close(); } catch {}
  if (pc) try { pc.close(); } catch {}
  if (micStream) micStream.getTracks().forEach((t) => t.stop());
  if (audioCtx) try { audioCtx.close(); } catch {}
  pc = dc = micStream = audioCtx = encoder = decoder = micPort = null;
  peer = 0;
  setStatus(`idle - ${why}`);
}

$("join").onclick = () => {
  $("join").disabled = true;
  $("leave").hidden = false;
  connect();
};
$("leave").onclick = () => {
  $("join").disabled = false;
  $("leave").hidden = true;
  hangup("left");
  if (ws) ws.close();
};

if (params.get("room")) $("room").value = params.get("room");
setStatus("idle");
