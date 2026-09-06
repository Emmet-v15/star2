// Drive the real Firefox through geckodriver's WebDriver HTTP API and run
// two guest pages through the join flow, capturing ICE candidates and state
// transitions. One geckodriver process per guest: ports 7000 and 7001.
const PORTS = ["http://127.0.0.1:7000", "http://127.0.0.1:7001"];
const URL = "http://localhost:8080/#rv=ws://localhost:9101&token=star2-dev";

const j = async (driver, method, path, body) => {
  const r = await fetch(driver + path, {
    method,
    body: body === undefined ? (method === "POST" ? "{}" : undefined) : JSON.stringify(body),
  });
  const v = await r.json();
  if (v.value && v.value.error) throw new Error(`${method} ${path}: ${JSON.stringify(v.value)}`);
  return v.value;
};

const HOOK = `
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

async function guest(i) {
  const driver = PORTS[i];
  const v = await j(driver, "POST", "/session", {
    capabilities: {
      alwaysMatch: {
        browserName: "firefox",
        "moz:firefoxOptions": {
          args: ["-headless"],
          prefs: {
            "media.navigator.streams.fake": true,
            "media.navigator.permission.disabled": true, "media.peerconnection.ice.no_host": true, "media.peerconnection.ice.default_address_only": true, "media.peerconnection.ice.proxy_only_if_behind_proxy": true,
          },
        },
      },
    },
  });
  const id = v.sessionId;
  if (!id) throw new Error(`no session id: ${JSON.stringify(v)}`);
  await j(driver, "POST", `/session/${id}/url`, { url: URL });
  await j(driver, "POST", `/session/${id}/execute/sync`, { script: HOOK, args: [] });
  return { driver, id };
}

const run = (g, script, args = []) =>
  j(g.driver, "POST", `/session/${g.id}/execute/sync`, { script, args });

const elId = (el) => {
  const key = Object.keys(el).find((k) => k.startsWith("element-"));
  return el[key];
};

const click = async (g, sel) => {
  const el = await j(g.driver, "POST", `/session/${g.id}/element`, { using: "css selector", value: sel });
  await j(g.driver, "POST", `/session/${g.id}/element/${elId(el)}/click`);
};

const fill = async (g, sel, text) => {
  const el = await j(g.driver, "POST", `/session/${g.id}/element`, { using: "css selector", value: sel });
  await j(g.driver, "POST", `/session/${g.id}/element/${elId(el)}/value`, { text });
};

const A = await guest(0);
const B = await guest(1);
console.log("two firefox guests loaded");

await click(A, "button");
await new Promise((r) => setTimeout(r, 2000));
const room = await run(A, "return localStorage.getItem('star2.room')");
console.log("room:", room);

await fill(B, "input", room ?? "");
await click(B, "button");

let ok = false;
for (let i = 0; i < 25; i++) {
  await new Promise((r) => setTimeout(r, 1000));
  const sA = await run(A, "return document.body.innerText.split('\\n')[2] ?? ''");
  const sB = await run(B, "return document.body.innerText.split('\\n')[2] ?? ''");
  if (/^connected/.test(sA ?? "") && /^connected/.test(sB ?? "")) {
    ok = true;
    console.log(`CONNECTED after ~${i + 1}s: A="${sA}" B="${sB}"`);
    break;
  }
  if (i === 24) console.log(`NOT CONNECTED after 25s: A="${sA}" B="${sB}"`);
}

for (const [i, g] of [A, B].entries()) {
  const log = await run(g, "return window.__rtc.join('\\n')");
  console.log(`\n===== guest ${String.fromCharCode(65 + i)} rtc log =====\n${log}`);
  const body = await run(g, "return document.body.innerText");
  console.log(`----- guest ${String.fromCharCode(65 + i)} page -----\n${body}`);
}

for (const g of [A, B]) await j(g.driver, "DELETE", `/session/${g.id}`);
process.exit(ok ? 0 : 1);
