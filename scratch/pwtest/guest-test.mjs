// Drive two browser instances through the guest join flow and capture every
// ICE candidate + state change, so a failed pairing shows exactly which pairs
// died. Usage: bun guest-test.mjs <ff|chromium> <ff|chromium>
import { firefox, chromium } from "playwright";

const KIND = { ff: firefox, chromium: chromium };
const [, , aKind = "ff", bKind = "ff"] = process.argv;
const URL = "http://localhost:8080/#rv=ws://localhost:9101&token=star2-dev";

const hooks = `
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

async function make(kind) {
  const browser = await KIND[kind].launch({
    headless: false,
    firefoxUserPrefs: {
      "media.navigator.streams.fake": true,
      "media.navigator.permission.disabled": true,
      "media.peerconnection.mdns_obfuscation.enabled": true,
    },
    args: kind === "chromium" ? ["--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream"] : [],
  });
  const page = await (await browser.newContext()).newPage();
  page.on("console", (m) => {
    const t = m.text();
    if (/ice|webrtc|candidate/i.test(t)) console.log(`[${kind} console] ${t}`);
  });
  await page.addInitScript(hooks);
  return { browser, page };
}

const A = await make(aKind);
const B = await make(bKind);

await A.page.goto(URL);
await A.page.waitForLoadState("domcontentloaded");
await A.page.evaluate(() => {
  const b = [...document.querySelectorAll("button")].find((x) => x.textContent === "Join");
  b.click();
});
await A.page.waitForTimeout(2000);
const room = await A.page.evaluate(() => localStorage.getItem("star2.room"));
console.log(`room: ${room}`);

await B.page.goto(URL);
await B.page.waitForLoadState("domcontentloaded");
await B.page.evaluate((room) => {
  const input = document.querySelector("input");
  input.value = room;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  const b = [...document.querySelectorAll("button")].find((x) => x.textContent === "Join");
  b.click();
}, room);

for (let i = 0; i < 25; i++) {
  await A.page.waitForTimeout(1000);
  const sa = await A.page.evaluate(() => document.body.innerText.split("\n")[2] ?? "");
  const sb = await B.page.evaluate(() => document.body.innerText.split("\n")[2] ?? "");
  if (/^connected/.test(sa) && /^connected/.test(sb)) {
    console.log(`CONNECTED after ~${i + 1}s: A="${sa}" B="${sb}"`);
    break;
  }
  if (i === 24) console.log(`NOT CONNECTED after 25s: A="${sa}" B="${sb}"`);
}

console.log(`\n===== ${aKind} (A) rtc log =====`);
console.log((await A.page.evaluate(() => window.__rtc)).join("\n"));
console.log(`\n===== ${bKind} (B) rtc log =====`);
console.log((await B.page.evaluate(() => window.__rtc)).join("\n"));

await A.browser.close();
await B.browser.close();
