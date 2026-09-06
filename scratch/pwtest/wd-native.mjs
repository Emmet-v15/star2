// One Firefox guest joins a room token given on argv (e.g. a native peer's).
// Usage: bun wd-native.mjs <room-token>
const DRIVER = "http://127.0.0.1:7000";
const URL = "http://localhost:8080/#rv=ws://localhost:9101&token=star2-dev";
const room = process.argv[2];
if (!room) throw new Error("usage: bun wd-native.mjs <room-token>");

const j = async (method, path, body) => {
  const r = await fetch(DRIVER + path, {
    method,
    body: body === undefined ? (method === "POST" ? "{}" : undefined) : JSON.stringify(body),
  });
  const v = await r.json();
  if (v.value && v.value.error) throw new Error(`${method} ${path}: ${JSON.stringify(v.value)}`);
  return v.value;
};

const HOOK = `
  const OrigDec = window.AudioDecoder;
  window.AudioDecoder = class extends OrigDec {
    decode(c) {
      try { super.decode(c); window.__rtc.push("decode ok: " + c.type); }
      catch (e) { window.__rtc.push("decode ERROR: " + e.message); }
    }
  };
  window.__rtc = [];
  const log = (m) => window.__rtc.push(m);
  const OrigPC = window.RTCPeerConnection;
  window.RTCPeerConnection = class extends OrigPC {
    constructor(...a) {
      super(...a);
      log("pc created");
      this.addEventListener("icecandidate", (e) => {
        if (e.candidate) log("cand: " + e.candidate.candidate);
        else log("cand: end-of-gather");
      });
      this.addEventListener("iceconnectionstatechange", () => log("ice: " + this.iceConnectionState));
      this.addEventListener("connectionstatechange", () => log("pc: " + this.connectionState));
    }
  };
`;

const v = await j("POST", "/session", {
  capabilities: {
    alwaysMatch: {
      browserName: "firefox",
      "moz:firefoxOptions": {
        args: ["-headless"],
        prefs: { "media.navigator.streams.fake": true, "media.navigator.permission.disabled": true, "media.peerconnection.ice.no_host": true, "media.peerconnection.ice.default_address_only": true, "media.peerconnection.ice.proxy_only_if_behind_proxy": true },
      },
    },
  },
});
const id = v.sessionId;
await j("POST", `/session/${id}/url`, { url: URL });
await j("POST", `/session/${id}/execute/sync`, { script: HOOK, args: [] });

const elId = (el) => {
  const key = Object.keys(el).find((k) => k.startsWith("element-"));
  return el[key];
};
const el = await j("POST", `/session/${id}/element`, { using: "css selector", value: "input" });
await j("POST", `/session/${id}/element/${elId(el)}/value`, { text: room });
const btn = await j("POST", `/session/${id}/element`, { using: "css selector", value: "button" });
await j("POST", `/session/${id}/element/${elId(btn)}/click`);
console.log("joining", room);

let status = "";
for (let i = 0; i < 25; i++) {
  await new Promise((r) => setTimeout(r, 1000));
  status = await j("POST", `/session/${id}/execute/sync`, {
    script: "return document.body.innerText.split('\\n')[2] ?? ''",
    args: [],
  });
  if (/^connected|^idle|^failed/i.test(status ?? "")) break;
}
console.log("final status:", status);

const canvas = await j("POST", `/session/${id}/execute/sync`, {
  script: "return (() => { const c = document.querySelector('canvas'); return c ? `${c.width}x${c.height}` : 'no canvas'; })()",
  args: [],
});
console.log("canvas:", canvas);

const log = await j("POST", `/session/${id}/execute/sync`, {
  script: "return window.__rtc.join('\\n')",
  args: [],
});
console.log("===== firefox guest rtc log =====\n" + log);

await j("DELETE", `/session/${id}`);
process.exit(/^connected/.test(status ?? "") ? 0 : 1);
