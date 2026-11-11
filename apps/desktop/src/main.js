// The HENOS window. It shows one snapshot of state from the Rust side and
// re-reads it whenever the "state-changed" event arrives.
//
// Device names come from other devices, so every piece of text is put on the
// page with textContent, never as HTML. The one exception is the QR code,
// which the Rust side draws itself.

"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

/** The latest snapshot from the Rust side. */
let state = null;

/** What only this window knows: which panel is open, what was typed. */
const ui = {
  panel: "none", // "none" | "join"
  selectedDevice: null,
  code: "",
  busy: false,
  error: "",
  confirmUnpair: null,
};

let countdownTimer = null;

/** Builds an element. Children may be elements, strings or null. */
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
    else if (key === "disabled" || key === "autofocus") el[key] = Boolean(value);
    else el.setAttribute(key, value);
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
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

function formatCode(code) {
  return code.slice(0, 3) + " " + code.slice(3);
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

// ---------- sections ----------

function header() {
  const paused = state.clipboardPaused;
  return h(
    "header",
    { class: "header" },
    h(
      "div",
      { class: "identity" },
      h("h1", {}, state.device.name),
      h("p", { class: "muted small" }, "This " + platformName(state.device.platform)),
    ),
    h(
      "button",
      {
        class: "toggle",
        "aria-pressed": String(!paused),
        onclick: () => run("set_clipboard_paused", { paused: !paused }),
      },
      h("span", { class: "dot " + (paused ? "warn" : "ok") }),
      paused ? "Clipboard sharing paused" : "Clipboard sharing on",
    ),
  );
}

function pairedStatus(device) {
  if (device.connected) return ["ok", "Connected"];
  if (device.nearby) return ["warn", "Nearby, connecting"];
  return ["", "Not on this network"];
}

function pairedList() {
  const rows = state.paired.map((device) => {
    const [dot, label] = pairedStatus(device);
    const confirming = ui.confirmUnpair === device.id;
    return h(
      "div",
      { class: "row" },
      h(
        "div",
        { class: "grow" },
        h("div", { class: "name" }, device.name),
        h(
          "div",
          { class: "status muted small" },
          h("span", { class: "dot " + dot }),
          platformName(device.platform) + " · " + label,
        ),
      ),
      h(
        "div",
        { class: "actions" },
        confirming
          ? [
              h(
                "button",
                {
                  class: "danger",
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
                class: "quiet",
                onclick: () => {
                  ui.confirmUnpair = device.id;
                  render();
                },
              },
              "Unpair",
            ),
      ),
    );
  });

  return h(
    "section",
    { class: "card" },
    h("h2", {}, "Your devices"),
    rows.length
      ? h("div", { class: "list" }, rows)
      : h("p", { class: "empty" }, "No devices paired yet. Add one below."),
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
    { class: "pairing" },
    qr,
    h(
      "div",
      { class: "details" },
      h("div", { class: "code" }, formatCode(pairing.code)),
      h(
        "p",
        { class: "muted" },
        "On the other device, choose Enter a code and type this, or scan the QR code with the phone app.",
      ),
      h("p", { class: "muted small" }, `Works once. Expires in ${minutes}:${seconds}.`),
      h("div", {}, h("button", { onclick: () => run("stop_pairing") }, "Cancel")),
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
    if (paired) {
      ui.panel = "none";
      ui.code = "";
      render();
    }
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
    { class: "join" },
    candidates.length
      ? h(
          "div",
          { class: "choices" },
          candidates.map((device) =>
            h(
              "button",
              {
                class: "choice" + (device.id === ui.selectedDevice ? " selected" : ""),
                "aria-pressed": String(device.id === ui.selectedDevice),
                onclick: () => {
                  ui.selectedDevice = device.id;
                  render();
                },
              },
              h("span", { class: "dot " + (device.pairing ? "ok" : "") }),
              h("span", {}, device.name),
              h(
                "span",
                { class: "muted small" },
                platformName(device.platform) + (device.pairing ? " · showing a code" : " · not showing a code yet"),
              ),
            ),
          ),
        )
      : h(
          "p",
          { class: "empty" },
          "No other HENOS device found on this network yet. Open HENOS on the other device, then choose Show a code there.",
        ),
    h(
      "div",
      { class: "entry" },
      input,
      h("button", { class: "primary", disabled: !ready, onclick: submit }, ui.busy ? "Pairing…" : "Pair"),
      h(
        "button",
        {
          onclick: () => {
            ui.panel = "none";
            ui.error = "";
            ui.code = "";
            render();
          },
        },
        "Cancel",
      ),
    ),
    ui.error ? h("p", { class: "error", role: "alert" }, ui.error) : null,
  );
}

function addDevice() {
  let body;
  if (state.pairing) {
    body = showCodePanel();
  } else if (ui.panel === "join") {
    body = joinPanel();
  } else {
    body = [
      h(
        "div",
        { class: "tabs" },
        h(
          "button",
          {
            class: "primary",
            disabled: ui.busy,
            onclick: () => run("start_pairing"),
          },
          "Show a code",
        ),
        h(
          "button",
          {
            onclick: () => {
              ui.panel = "join";
              ui.error = "";
              render();
            },
          },
          "Enter a code",
        ),
      ),
      ui.error ? h("p", { class: "error", role: "alert" }, ui.error) : null,
    ];
  }
  return h("section", { class: "card" }, h("h2", {}, "Add a device"), body);
}

function activity() {
  if (!state.activity.length) return null;
  return h(
    "section",
    { class: "card activity" },
    h("h2", {}, "Recent activity"),
    h(
      "div",
      { class: "list" },
      state.activity.slice(0, 12).map((entry) =>
        h("div", { class: "row" }, h("time", {}, formatTime(entry.atMs)), h("span", {}, entry.text)),
      ),
    ),
  );
}

function footer() {
  return h(
    "p",
    { class: "footer muted small" },
    `HENOS ${state.version} · One Digital World · devices connect directly on your network, end-to-end encrypted`,
  );
}

// ---------- rendering ----------

function render() {
  if (!state) return;
  const root = document.getElementById("app");

  // Rebuilding the page must not interrupt typing.
  const active = document.activeElement;
  const typing = active && active.id === "code-input";
  const caret = typing ? active.selectionStart : null;

  root.replaceChildren(header(), pairedList(), addDevice(), activity() || "", footer());

  if (typing) {
    const input = document.getElementById("code-input");
    if (input) {
      input.focus();
      input.setSelectionRange(caret, caret);
    }
  }

  // Tick the countdown while a code is on show.
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

listen("state-changed", refresh);
refresh().catch(showStartupError);
