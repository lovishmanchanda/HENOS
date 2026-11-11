// Stand-in for the Rust side, so the window can be opened in an ordinary
// browser while working on its looks. Not part of the shipped app.
"use strict";

(() => {
  const listeners = [];
  const notify = () => listeners.forEach((fn) => fn());
  const id = (n) => String(n).repeat(64).slice(0, 64);

  const fake = {
    version: "0.1.0",
    device: { id: id("a1"), name: "My MacBook Pro", platform: "macos" },
    paired: [
      { id: id("b2"), name: "Pixel 9", platform: "android", connected: true, nearby: true },
      { id: id("c3"), name: "DESKTOP-GAMING", platform: "windows", connected: false, nearby: true },
      { id: id("d4"), name: "Old ThinkPad <script>alert(1)</script>", platform: "linux", connected: false, nearby: false },
    ],
    nearby: [
      { id: id("e5"), name: "Living room PC", platform: "windows", pairing: true },
      { id: id("f6"), name: "Sam's Mac", platform: "macos", pairing: false },
    ],
    pairing: null,
    clipboardPaused: false,
    activity: [
      { atMs: Date.now() - 60_000, kind: "clipboardReceived", text: "Received copied text (42 bytes) from Pixel 9" },
      { atMs: Date.now() - 300_000, kind: "clipboardSent", text: "Sent copied text (1.2 KB) to 1 device" },
      { atMs: Date.now() - 3_600_000, kind: "paired", text: "Paired with Pixel 9" },
    ],
  };
  let expires = 0;

  function qr() {
    // A checkerboard, just to fill the space.
    let cells = "";
    for (let y = 0; y < 25; y++)
      for (let x = 0; x < 25; x++)
        if ((x * 7 + y * 13 + x * y) % 3 === 0) cells += `<rect x="${x}" y="${y}" width="1" height="1"/>`;
    return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 25 25"><rect width="25" height="25" fill="#fff"/>${cells}</svg>`;
  }

  const commands = {
    get_state() {
      if (fake.pairing) {
        fake.pairing.secondsLeft = Math.max(0, Math.round((expires - Date.now()) / 1000));
        if (fake.pairing.secondsLeft === 0) fake.pairing = null;
      }
      return structuredClone(fake);
    },
    start_pairing() {
      expires = Date.now() + 120_000;
      fake.pairing = { code: "482915", ticket: "henos://pair?v=1", qrSvg: qr(), secondsLeft: 120 };
      notify();
      return fake.pairing;
    },
    stop_pairing() {
      fake.pairing = null;
      notify();
    },
    async pair_with_code({ deviceId, code }) {
      await new Promise((r) => setTimeout(r, 600));
      if (code !== "123456")
        throw "That code is not right. The other device has discarded it, so ask it for a new one.";
      const device = fake.nearby.find((d) => d.id === deviceId);
      fake.nearby = fake.nearby.filter((d) => d.id !== deviceId);
      const paired = { id: device.id, name: device.name, platform: device.platform, connected: true, nearby: true };
      fake.paired.unshift(paired);
      fake.activity.unshift({ atMs: Date.now(), kind: "paired", text: "Paired with " + device.name });
      notify();
      return paired;
    },
    unpair({ deviceId }) {
      const device = fake.paired.find((d) => d.id === deviceId);
      fake.paired = fake.paired.filter((d) => d.id !== deviceId);
      fake.activity.unshift({ atMs: Date.now(), kind: "unpaired", text: "No longer paired with " + device.name });
      notify();
    },
    set_clipboard_paused({ paused }) {
      fake.clipboardPaused = paused;
      notify();
    },
  };

  window.__TAURI__ = {
    core: { invoke: async (name, args) => commands[name](args || {}) },
    event: { listen: async (_event, fn) => listeners.push(fn) },
  };
  if (new URLSearchParams(location.search).has("empty")) {
    fake.paired = [];
    fake.nearby = [];
    fake.activity = [];
  }
})();
