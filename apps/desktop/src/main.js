// The HENOS window. It shows one snapshot of state from the Rust side and
// re-reads it whenever the "state-changed" event arrives. It does nothing on
// its own while idle: no timers run unless a pairing code is on screen.
//
// Device names come from other devices, so every piece of text is put on the
// page with textContent, never as HTML. The exceptions are the QR code, which
// the Rust side draws itself, and the fixed icons below.

"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

/** The latest snapshot from the Rust side. */
let state = null;

/** What only this window knows: which panel is open, what was typed. */
const ui = {
  panel: "none", // "none" | "choose" | "join"
  selectedDevice: null,
  code: "",
  busy: false,
  error: "",
  confirmUnpair: null,
  dropTarget: null,
};

let countdownTimer = null;

const ICONS = {
  computer:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="5" width="16" height="11" rx="2"/><path d="M2.5 19.5h19"/></svg>',
  phone:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><rect x="7" y="3" width="10" height="18" rx="2.5"/><path d="M11 18h2"/></svg>',
  file:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M13 3H8a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V8z"/><path d="M13 3v5h5"/></svg>',
  plus:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="12" cy="12" r="9"/><path d="M12 8v8M8 12h8"/></svg>',
};

/** Builds an element. Children may be elements, strings or null. */
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
    else if (key === "disabled") el.disabled = Boolean(value);
    else el.setAttribute(key, value);
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

/** One of the fixed icons above. */
function glyph(name) {
  const el = h("span", { class: "glyph", "aria-hidden": "true" });
  el.innerHTML = ICONS[name];
  return el;
}

const PLATFORM_NAMES = {
  macos: "Mac",
  windows: "Windows PC",
  linux: "Linux PC",
  android: "Android",
  ios: "iPhone or iPad",
};

function platformName(platform) {
  return PLATFORM_NAMES[platform] || "Device";
}

function platformGlyph(platform) {
  return glyph(platform === "android" || platform === "ios" ? "phone" : "computer");
}

function formatCode(code) {
  return code.slice(0, 3) + " " + code.slice(3);
}

function formatSize(bytes) {
  if (bytes < 1024) return bytes + " bytes";
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return (value >= 100 ? value.toFixed(0) : value.toFixed(1)) + " " + units[unit];
}

function formatTime(ms) {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

async function refresh() {
  state = await invoke("get_state");
  render();
}

/** Runs a command, showing its error in the open panel if it fails. */
async function run(command, args) {
  ui.busy = true;
  ui.error = "";
  render();
  try {
    return await invoke(command, args);
  } catch (error) {
    ui.error = String(error);
    return undefined;
  } finally {
    ui.busy = false;
    await refresh();
  }
}

function closePanel() {
  ui.panel = "none";
  ui.error = "";
  ui.code = "";
  render();
}

// ---------- sections ----------

function summary() {
  if (state.clipboardPaused) return "Sharing is paused";
  const connected = state.paired.filter((device) => device.connected).length;
  if (state.paired.length === 0) return "Add a device to start sharing";
  if (connected === 0) return "None of your devices are nearby";
  return connected === 1 ? "Sharing with 1 device" : `Sharing with ${connected} devices`;
}

function topBar() {
  const sharing = !state.clipboardPaused;
  return h(
    "header",
    { class: "top" },
    h(
      "div",
      { class: "who" },
      h("div", { class: "eyebrow" }, "This " + platformName(state.device.platform)),
      h("h1", {}, state.device.name),
      h("p", { class: "summary" }, summary()),
    ),
    h(
      "button",
      {
        class: "switch",
        role: "switch",
        "aria-checked": String(sharing),
        "aria-label": "Share the clipboard",
        onclick: () => run("set_clipboard_paused", { paused: sharing }),
      },
      h("span", { class: "track" }),
    ),
  );
}

function pairedStatus(device) {
  if (device.connected) return ["ok", "Connected"];
  if (device.nearby) return ["wait", "Connecting"];
  return ["", "Not nearby"];
}

function deviceRow(device) {
  const [dot, label] = pairedStatus(device);
  const confirming = ui.confirmUnpair === device.id;
  return h(
    "div",
    {
      class: "row" + (ui.dropTarget === device.id ? " dropping" : ""),
      // Lets a dragged file find the device it is dropped on.
      "data-device": device.connected ? device.id : null,
    },
    platformGlyph(device.platform),
    h(
      "div",
      { class: "text" },
      h("div", { class: "name" }, device.name),
      h(
        "div",
        { class: "sub" },
        h("span", { class: "dot " + dot }),
        ui.dropTarget === device.id ? "Drop to send" : label,
      ),
    ),
    h(
      "div",
      { class: "side" },
      device.connected && !confirming
        ? h(
            "button",
            {
              class: "btn",
              "aria-label": "Send a file to " + device.name,
              onclick: () => run("pick_and_send", { deviceId: device.id }),
            },
            "Send file",
          )
        : null,
      confirming
        ? [
            h(
              "button",
              {
                class: "btn danger",
                onclick: async () => {
                  ui.confirmUnpair = null;
                  await run("unpair", { deviceId: device.id });
                },
              },
              "Unpair",
            ),
            h(
              "button",
              {
                class: "btn",
                onclick: () => {
                  ui.confirmUnpair = null;
                  render();
                },
              },
              "Keep",
            ),
          ]
        : h(
            "button",
            {
              class: "btn ghost",
              "aria-label": "Unpair " + device.name,
              onclick: () => {
                ui.confirmUnpair = device.id;
                render();
              },
            },
            "Unpair",
          ),
    ),
  );
}

function showCodePanel() {
  const pairing = state.pairing;
  const qr = h("div", { class: "qr", role: "img", "aria-label": "Pairing QR code" });
  // Drawn by the Rust side from this device's own id and code.
  qr.innerHTML = pairing.qrSvg;
  const minutes = Math.floor(pairing.secondsLeft / 60);
  const seconds = String(pairing.secondsLeft % 60).padStart(2, "0");
  return h(
    "div",
    { class: "panel" },
    h(
      "div",
      { class: "show" },
      qr,
      h(
        "div",
        { class: "details" },
        h("div", { class: "code" }, formatCode(pairing.code)),
        h("p", { class: "hint" }, "Scan with the phone app, or choose Enter a code on the other device."),
        h("p", { class: "fine" }, `Works once · expires in ${minutes}:${seconds}`),
        h("div", {}, h("button", { class: "btn", onclick: () => run("stop_pairing") }, "Cancel")),
      ),
    ),
  );
}

function joinPanel() {
  // Every nearby device is listed, the ones showing a code first, so the
  // person can see the other device was found even before it shows a code.
  const candidates = state.nearby;
  if (!candidates.some((device) => device.id === ui.selectedDevice)) {
    const showing = candidates.filter((device) => device.pairing);
    ui.selectedDevice =
      showing.length === 1 ? showing[0].id : candidates.length === 1 ? candidates[0].id : null;
  }
  const digits = ui.code.replace(/\D/g, "");
  const ready = Boolean(ui.selectedDevice) && digits.length === 6 && !ui.busy;

  const submit = async () => {
    if (!ready) return;
    const paired = await run("pair_with_code", { deviceId: ui.selectedDevice, code: digits });
    if (paired) closePanel();
  };

  const input = h("input", {
    class: "code-input",
    id: "code-input",
    type: "text",
    inputmode: "numeric",
    autocomplete: "off",
    maxlength: "7",
    placeholder: "000 000",
    "aria-label": "Pairing code",
    value: ui.code,
    oninput: (event) => {
      ui.code = event.target.value;
      ui.error = "";
      render();
    },
    onkeydown: (event) => {
      if (event.key === "Enter") submit();
    },
  });

  return h(
    "div",
    { class: "panel" },
    candidates.length
      ? h(
          "div",
          { class: "nested" },
          candidates.map((device) =>
            h(
              "button",
              {
                class: "row choice",
                "aria-pressed": String(device.id === ui.selectedDevice),
                onclick: () => {
                  ui.selectedDevice = device.id;
                  render();
                },
              },
              platformGlyph(device.platform),
              h(
                "div",
                { class: "text" },
                h("div", { class: "name" }, device.name),
                h(
                  "div",
                  { class: "sub" },
                  h("span", { class: "dot " + (device.pairing ? "ok" : "") }),
                  device.pairing ? "Showing a code" : "Not showing a code yet",
                ),
              ),
            ),
          ),
        )
      : h(
          "p",
          { class: "hint" },
          "No other HENOS device found on this network yet. Open HENOS on the other device and choose Show a code.",
        ),
    h(
      "div",
      { class: "entry" },
      input,
      h("button", { class: "btn primary", disabled: !ready, onclick: submit }, ui.busy ? "Pairing…" : "Pair"),
      h("button", { class: "btn", onclick: closePanel }, "Cancel"),
    ),
    ui.error ? h("p", { class: "error", role: "alert" }, ui.error) : null,
  );
}

function choosePanel() {
  return h(
    "div",
    { class: "panel" },
    h("p", { class: "hint" }, "One device shows a code, the other enters it."),
    h(
      "div",
      { class: "choices" },
      h(
        "button",
        { class: "btn primary", disabled: ui.busy, onclick: () => run("start_pairing") },
        "Show a code",
      ),
      h(
        "button",
        {
          class: "btn",
          onclick: () => {
            ui.panel = "join";
            ui.error = "";
            lookNow();
            render();
          },
        },
        "Enter a code",
      ),
      h("button", { class: "btn ghost", onclick: closePanel }, "Cancel"),
    ),
    ui.error ? h("p", { class: "error", role: "alert" }, ui.error) : null,
  );
}

function devices() {
  let adding;
  if (state.pairing) adding = showCodePanel();
  else if (ui.panel === "join") adding = joinPanel();
  else if (ui.panel === "choose") adding = choosePanel();
  else {
    adding = h(
      "button",
      {
        class: "row",
        onclick: () => {
          ui.panel = "choose";
          ui.error = "";
          lookNow();
          render();
        },
      },
      glyph("plus"),
      h("span", { class: "text" }, "Add a device"),
    );
  }

  return h(
    "section",
    { class: "group" },
    h("h2", {}, "Devices"),
    h("div", { class: "sheet" }, state.paired.map(deviceRow), adding),
  );
}

function transferRow(transfer) {
  const who = transfer.deviceName;
  let sub;
  let side;
  const dismiss = h(
    "button",
    {
      class: "btn ghost",
      "aria-label": "Remove from list",
      onclick: () => run("dismiss_transfer", { transferId: transfer.id }),
    },
    "Clear",
  );
  const cancel = h(
    "button",
    { class: "btn", onclick: () => run("cancel_transfer", { transferId: transfer.id }) },
    "Cancel",
  );

  switch (transfer.status) {
    case "offered":
      sub = `${who} wants to send this · ${formatSize(transfer.size)}`;
      side = [
        h(
          "button",
          { class: "btn primary", onclick: () => run("accept_file", { transferId: transfer.id }) },
          "Accept",
        ),
        h(
          "button",
          { class: "btn", onclick: () => run("decline_file", { transferId: transfer.id }) },
          "Decline",
        ),
      ];
      break;
    case "waiting":
      sub = `Waiting for ${who} to accept · ${formatSize(transfer.size)}`;
      side = cancel;
      break;
    case "active": {
      const percent = transfer.size ? Math.floor((transfer.bytes / transfer.size) * 100) : 100;
      const bar = h("span", { class: "bar", role: "progressbar", "aria-valuenow": String(percent) }, h("span", {}));
      bar.firstChild.style.width = percent + "%";
      sub = [
        `${transfer.incoming ? "Receiving from" : "Sending to"} ${who} · ${percent}%`,
        bar,
      ];
      side = cancel;
      break;
    }
    case "done":
      sub = transfer.incoming
        ? `From ${who} · saved in Downloads, in the HENOS folder`
        : `Sent to ${who} · ${formatSize(transfer.size)}`;
      side = dismiss;
      break;
    default:
      sub = transfer.detail || "Did not complete.";
      side = dismiss;
  }

  return h(
    "div",
    { class: "row" + (transfer.status === "failed" ? " failed" : "") },
    glyph("file"),
    h("div", { class: "text" }, h("div", { class: "name" }, transfer.name), h("div", { class: "sub wrap" }, sub)),
    h("div", { class: "side" }, side),
  );
}

function transfers() {
  if (!state.transfers.length) return null;
  return h(
    "section",
    { class: "group" },
    h("h2", {}, "Files"),
    h("div", { class: "sheet" }, state.transfers.map(transferRow)),
  );
}

function activity() {
  if (!state.activity.length) return null;
  return h(
    "section",
    { class: "group activity" },
    h("h2", {}, "Recent"),
    h(
      "div",
      { class: "sheet" },
      state.activity.slice(0, 8).map((entry) =>
        h(
          "div",
          { class: "row" },
          h("time", {}, formatTime(entry.atMs)),
          h("span", { class: "what" }, entry.text),
        ),
      ),
    ),
  );
}

function foot() {
  return h("p", { class: "foot" }, `HENOS ${state.version} · One Digital World · end-to-end encrypted`);
}

// ---------- rendering ----------

function render() {
  if (!state) return;
  const root = document.getElementById("app");

  // Rebuilding the page must not interrupt typing.
  const active = document.activeElement;
  const typing = active && active.id === "code-input";
  const caret = typing ? active.selectionStart : null;

  root.replaceChildren(topBar(), transfers() || "", devices(), activity() || "", foot());

  if (typing) {
    const input = document.getElementById("code-input");
    if (input) {
      input.focus();
      input.setSelectionRange(caret, caret);
    }
  }

  // The only timer: ticking the countdown while a code is on show.
  if (state.pairing && !countdownTimer) {
    countdownTimer = setInterval(refresh, 1000);
  } else if (!state.pairing && countdownTimer) {
    clearInterval(countdownTimer);
    countdownTimer = null;
  }
}

function showStartupError(error) {
  document
    .getElementById("app")
    .replaceChildren(h("p", { class: "error", role: "alert" }, "HENOS could not start: " + String(error)));
}

/** Asks the Rust side to search for devices right now. */
function lookNow() {
  invoke("look_now").catch(() => {});
}

// ---------- dragging files onto a device ----------

/** The connected device under a point of the window, if any. */
function deviceAt(position) {
  // The position arrives in screen pixels; the page thinks in CSS pixels.
  const scale = window.devicePixelRatio || 1;
  const el = document.elementFromPoint(position.x / scale, position.y / scale);
  const row = el && el.closest("[data-device]");
  return row ? row.getAttribute("data-device") : null;
}

function watchFileDrops() {
  const webview = window.__TAURI__.webview;
  if (!webview) return;
  webview.getCurrentWebview().onDragDropEvent((event) => {
    const drag = event.payload;
    let target = null;
    if (drag.type === "over" || drag.type === "enter") {
      target = deviceAt(drag.position);
    } else if (drag.type === "drop") {
      const device = deviceAt(drag.position);
      if (device) {
        for (const path of drag.paths) {
          invoke("send_file", { deviceId: device, path }).catch((error) => {
            ui.error = String(error);
            render();
          });
        }
      }
    }
    if (target !== ui.dropTarget) {
      ui.dropTarget = target;
      render();
    }
  });
}

watchFileDrops();
listen("state-changed", refresh);
// Coming back to the window is a good moment to look for devices.
window.addEventListener("focus", lookNow);
refresh().catch(showStartupError);
