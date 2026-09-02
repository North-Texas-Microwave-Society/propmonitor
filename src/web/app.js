// propmonitor browser client. Subscribes to /ws for live waterfall + raw
// dBFS + measurements. Loads /api/config at boot, lets the user PUT it.
// All state is kept in module-level vars; nothing fancy.

const $ = (id) => document.getElementById(id);

const FFT_N = 1024;
const WATERFALL_ROWS = 240;
const SPAN_DB = 30; // dynamic range of the colormap, in dB above per-row floor

let canvas, ctx;
let device = null;
let lastRaw = null;
let beaconOffsetHz = 0; // for the vertical marker
let sampleRate = 250000;

// 8-stop magma-like ramp. Each entry: [r, g, b] for normalized t in [0, 1].
const COLOR_STOPS = [
  [  8,   8,  20],
  [ 24,  16,  56],
  [ 64,  16,  88],
  [120,  20,  96],
  [180,  44,  78],
  [222,  92,  52],
  [248, 168,  72],
  [255, 240, 220],
];

function colorFor(t) {
  if (t <= 0) return COLOR_STOPS[0];
  if (t >= 1) return COLOR_STOPS[COLOR_STOPS.length - 1];
  const last = COLOR_STOPS.length - 1;
  const scaled = t * last;
  const i = Math.floor(scaled);
  const f = scaled - i;
  const a = COLOR_STOPS[i];
  const b = COLOR_STOPS[i + 1];
  return [
    Math.round(a[0] + (b[0] - a[0]) * f),
    Math.round(a[1] + (b[1] - a[1]) * f),
    Math.round(a[2] + (b[2] - a[2]) * f),
  ];
}

function initCanvas() {
  canvas = $("waterfall");
  // Render at internal resolution = FFT_N wide, WATERFALL_ROWS tall.
  // CSS stretches to full width. image-rendering: pixelated keeps it crisp.
  canvas.width = FFT_N;
  canvas.height = WATERFALL_ROWS;
  ctx = canvas.getContext("2d");
  ctx.fillStyle = "#06060a";
  ctx.fillRect(0, 0, FFT_N, WATERFALL_ROWS);
}

function scrollAndDrawRow(bins, f0_hz, bin_hz) {
  // Scroll existing pixels up one row.
  const imgData = ctx.getImageData(0, 1, FFT_N, WATERFALL_ROWS - 1);
  ctx.putImageData(imgData, 0, 0);

  // Compute floor (median) and span for normalization.
  const dbs = new Float32Array(FFT_N);
  for (let i = 0; i < FFT_N; i++) {
    dbs[i] = 10 * Math.log10(Math.max(bins[i], 1e-30));
  }
  const sorted = dbs.slice().sort();
  const floor = sorted[FFT_N >> 1];

  // Draw new row at the bottom.
  const row = ctx.createImageData(FFT_N, 1);
  for (let i = 0; i < FFT_N; i++) {
    const above = dbs[i] - floor;
    const t = Math.max(0, Math.min(1, above / SPAN_DB));
    const [r, g, b] = colorFor(t);
    const o = i * 4;
    row.data[o] = r;
    row.data[o + 1] = g;
    row.data[o + 2] = b;
    row.data[o + 3] = 255;
  }
  ctx.putImageData(row, 0, WATERFALL_ROWS - 1);

  // Draw marker for beacon offset (overlay; not in image data).
  if (beaconOffsetHz !== null && bin_hz > 0) {
    const markerBin = Math.round((beaconOffsetHz - f0_hz) / bin_hz);
    if (markerBin >= 0 && markerBin < FFT_N) {
      ctx.fillStyle = "rgba(0, 200, 255, 0.4)";
      ctx.fillRect(markerBin, 0, 1, WATERFALL_ROWS);
    }
  }

  updateFreqAxis(f0_hz, bin_hz);
}

function updateFreqAxis(f0_hz, bin_hz) {
  const axis = $("freq-axis");
  // 5 labels equally spaced across the band.
  const labels = [];
  for (let i = 0; i < 5; i++) {
    const frac = i / 4;
    const hz = f0_hz + bin_hz * FFT_N * frac;
    const kHz = hz / 1000;
    labels.push(`${kHz >= 0 ? "+" : ""}${kHz.toFixed(1)} kHz`);
  }
  axis.innerHTML = labels.map((s) => `<span>${s}</span>`).join("");
}

function updateHeader() {
  const parts = [];
  if (device) {
    parts.push(`${(device.actual_frequency / 1e6).toFixed(6)} MHz`);
    parts.push(`sr ${Math.round(device.actual_sample_rate)} Hz`);
    parts.push(`gain ${device.actual_gain.toFixed(1)} dB`);
    if (device.gain_elements && device.gain_elements.length) {
      parts.push(`[${device.gain_elements.join(",")}]`);
    }
    sampleRate = device.actual_sample_rate;
  } else {
    parts.push("waiting for device…");
  }
  $("hdr-summary").textContent = parts.join(" · ");
  $("hdr-raw").textContent = lastRaw !== null ? `raw ${lastRaw.toFixed(1)} dBFS` : "";
}

function setReadout(m) {
  $("ro-noise").textContent = m.noise_floor_dbfs.toFixed(1) + " dBFS";
  $("ro-peak").textContent = m.signal_peak_dbfs.toFixed(1) + " dBFS";
  $("ro-avg").textContent = m.signal_avg_dbfs.toFixed(1) + " dBFS";
  $("ro-snr-peak").textContent = "+" + m.snr_peak_db.toFixed(1) + " dB";
  $("ro-meas-at").textContent = "at " + m.measured_at;
}

function setUploadStatus(ev) {
  $("ro-upload").textContent = `uploader: ${ev.status} at ${ev.at} · queue ${ev.queued}`;
}

// Last /api/update snapshot. WebSocket `update` frames patch the volatile
// fields onto it; everything else (the running build, the install path)
// only changes when the daemon restarts.
let updateInfo = null;

function shortCommit(sha) {
  if (!sha) return "unknown";
  return sha === "dev" ? "dev" : sha.slice(0, 7);
}

function buildLabel(b) {
  return `${b.version} (${shortCommit(b.commit)})`;
}

function renderUpdate() {
  const u = updateInfo;
  if (!u) return;
  const running = buildLabel(u.current);
  let text;
  switch (u.phase) {
    case "checking":
      text = "checking the release channel…";
      break;
    case "downloading":
      text = `downloading ${u.latest ? buildLabel(u.latest) : "the new build"}…`;
      break;
    case "installing":
      text = "installing — the daemon restarts itself in a moment";
      break;
    default:
      if (u.last_error) {
        text = `update: ${u.last_error}`;
      } else if (u.latest) {
        text = `update available: ${buildLabel(u.latest)} · running ${running}`;
        if (!u.current.can_self_update) {
          text += " · self-update needs a Linux install";
        } else if (!u.auto || !u.current.dist_build) {
          text += " · press Install";
        }
      } else if (u.last_check_at) {
        text = `up to date: ${running} · checked ${u.last_check_at}`;
      } else {
        text = `running ${running}`;
      }
  }
  $("ro-update").textContent = text;
  $("ro-update").className = u.last_error ? "err" : "muted";

  const busy = u.phase !== "idle";
  $("btn-check").disabled = busy;
  $("btn-install").disabled = busy || !u.latest || !u.current.can_self_update;
}

async function loadUpdate() {
  try {
    const r = await fetch("/api/update");
    if (r.ok) {
      updateInfo = await r.json();
      renderUpdate();
    }
  } catch (e) {}
}

// The channel task pushes every transition, so an install started from
// another browser tab (or by the timer) shows up here too. Before the
// first snapshot there is nothing to patch — fetch one rather than drop
// the event, or a single failed fetch at boot would leave this section
// dead until the operator reloads.
function applyUpdateEvent(ev) {
  if (!updateInfo) {
    loadUpdate();
    return;
  }
  updateInfo.phase = ev.phase;
  updateInfo.latest = ev.latest;
  updateInfo.last_error = ev.error;
  renderUpdate();
}

async function requestUpdate(path) {
  try {
    const r = await fetch(path, { method: "POST" });
    const body = await r.json();
    if (r.ok) {
      updateInfo = body;
      renderUpdate();
    } else {
      $("ro-update").textContent = `update: ${body.error || "request failed"}`;
      $("ro-update").className = "err";
    }
  } catch (e) {
    $("ro-update").textContent = "update: the daemon did not answer";
    $("ro-update").className = "err";
  }
}

// The daemon sends a heartbeat every 10 s on top of whatever the worker
// is producing, so a live socket is never quiet for long — even with the
// SDR down. A suspended laptop, or a NAT that forgets the flow, can
// leave a half-open socket the browser never reports as closed, and that
// is the one failure this page cannot recover from on its own. So treat
// silence past two missed beats as death and redial; the daemon replays
// a full snapshot on connect, which makes a needless reconnect cost one
// handshake and nothing else.
const WS_SILENCE_MS = 25000;
const WS_RETRY_MS = 1500;

let ws = null;
let silenceTimer = null;
let retryTimer = null;

function armSilenceTimer() {
  clearTimeout(silenceTimer);
  silenceTimer = setTimeout(() => {
    // Don't wait for onclose: a dead socket can sit in CLOSING for a
    // while. Detach it, forget it, dial again now.
    dropSocket();
    $("hdr-summary").textContent = "reconnecting…";
    connectWS();
  }, WS_SILENCE_MS);
}

function dropSocket() {
  clearTimeout(silenceTimer);
  const dead = ws;
  ws = null;
  if (!dead) return;
  dead.onopen = dead.onmessage = dead.onclose = dead.onerror = null;
  try { dead.close(); } catch (e) {}
}

function scheduleReconnect() {
  if (retryTimer) return;
  retryTimer = setTimeout(() => {
    retryTimer = null;
    connectWS();
  }, WS_RETRY_MS);
}

function connectWS() {
  clearTimeout(retryTimer);
  retryTimer = null;
  const url = (location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws";
  const sock = new WebSocket(url);
  ws = sock;
  sock.onopen = () => {
    $("hdr-summary").textContent = "connected";
    armSilenceTimer();
    // The connect snapshot carries device, raw level, last measurement,
    // uploader status and config. The update section is the exception:
    // its state includes the *running* build, which a self-update swaps
    // out from under an open page, so re-read it on every connect.
    loadUpdate();
  };
  sock.onclose = () => {
    if (ws !== sock) return; // superseded by a reconnect already in flight
    clearTimeout(silenceTimer);
    $("hdr-summary").textContent = "disconnected — reconnecting…";
    scheduleReconnect();
  };
  sock.onmessage = (msg) => {
    armSilenceTimer();
    let ev;
    try { ev = JSON.parse(msg.data); } catch (e) { return; }
    switch (ev.type) {
      case "device_info": device = ev; updateHeader(); break;
      case "raw_level":   lastRaw = ev.dbfs; updateHeader(); break;
      case "waterfall":   scrollAndDrawRow(ev.bins, ev.f0_hz, ev.bin_hz); break;
      case "measurement": setReadout(ev); break;
      case "upload":      setUploadStatus(ev); break;
      case "update":      applyUpdateEvent(ev); break;
      case "config":      onRemoteConfig(ev.config); break;
      case "heartbeat": break; // liveness only; the timer above is the point
      case "period_started": break;
      case "error":       $("hdr-summary").textContent = "error: " + ev.message; break;
    }
  };
}

// Select `value` in the device dropdown, adding an entry for a device
// that isn't currently plugged in so the form still reflects what the
// daemon is configured with.
function ensureDriverOption(value) {
  const sel = $("settings-form").driver;
  if (value && ![...sel.options].some((o) => o.value === value)) {
    const opt = document.createElement("option");
    opt.value = value;
    opt.textContent = `${value} (not connected)`;
    sel.appendChild(opt);
  }
  if (!sel.options.length) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "no SoapySDR devices found";
    opt.disabled = true;
    sel.appendChild(opt);
  }
  sel.value = value || "";
}

// Rescans USB, which costs a few hundred ms on the daemon — so only at
// boot and when the operator presses ↻. A config arriving over the
// WebSocket reuses the options already listed.
async function loadDevices(selected) {
  const sel = $("settings-form").driver;
  // Preserve the currently-saved value so it stays available even when
  // the device isn't currently plugged in.
  const current = selected || sel.value || "";
  sel.innerHTML = "";
  try {
    const r = await fetch("/api/devices");
    if (r.ok) {
      const { devices } = await r.json();
      for (const d of devices) {
        const opt = document.createElement("option");
        opt.value = d.value;
        opt.textContent = `${d.label} — ${d.value}`;
        sel.appendChild(opt);
      }
    }
  } catch (e) {}
  ensureDriverOption(current);
}

// True once the operator has typed in the settings form without saving.
// A config that arrives mid-edit must not overwrite half-finished work,
// so it gets announced instead of applied.
let formDirty = false;

function applyConfig(cfg) {
  const f = $("settings-form");
  ensureDriverOption(cfg.driver || "");
  f.frequency.value = cfg.frequency || "";
  f.mode.value = cfg.mode || "beacon";
  f.sample_rate.value = cfg.sample_rate || 250000;
  f.gain.value = cfg.gain != null ? cfg.gain : "";
  f.ppm.value = cfg.ppm || 0;
  // period_seconds is intentionally not in the form — it's a rarely-changed
  // value (the upload cadence the microwaveprop rate-limit assumes). Edit
  // config.yaml directly to override the 60 s default.
  if (cfg.beacon) {
    f.beacon_offset_hz.value = cfg.beacon.offset_hz;
    f.beacon_bandwidth_hz.value = cfg.beacon.bandwidth_hz;
    beaconOffsetHz = cfg.beacon.offset_hz;
  }
  f.http_bind.value = (cfg.http && cfg.http.bind) || "0.0.0.0:5760";
  if (cfg.microwaveprop) {
    f.mw_enabled.checked = cfg.microwaveprop.enabled !== false;
    f.mw_token.value = cfg.microwaveprop.monitor_token || "";
    f.mw_beacon_id.value = cfg.microwaveprop.beacon_id || "";
    f.mw_gridsquare.value = cfg.microwaveprop.gridsquare || "";
  } else {
    f.mw_enabled.checked = false;
  }
  const up = cfg.update || {};
  f.up_enabled.checked = up.enabled !== false;
  f.up_auto.checked = up.auto !== false;
  // Stored in seconds, shown in minutes: an hour is the useful unit here.
  f.up_interval.value = Math.max(1, Math.round((up.check_interval || 3600) / 60));
  formDirty = false;
}

// Boot path only — the WebSocket keeps the form current after this, and
// its connect snapshot usually delivers the same config before this
// fetch and its device scan (a few hundred ms of USB probing on the
// daemon, longer when a driver is present but the dongle isn't) return.
// By then the operator can already be typing, so the dirty check is
// re-read after every await: a late boot response must not wipe an edit
// that started while it was in flight.
async function loadConfig() {
  const r = await fetch("/api/config");
  if (!r.ok) return;
  const cfg = await r.json();
  // `undefined` keeps whatever is selected instead of forcing the saved
  // driver over a device the operator just picked.
  await loadDevices(formDirty ? undefined : cfg.driver || "");
  if (!formDirty) applyConfig(cfg);
}

// A `config` frame means the running config changed: another browser on
// the LAN saved, something curled PUT /api/config, or microwaveprop
// pushed a managed config down.
function onRemoteConfig(cfg) {
  // The marker tracks the daemon's passband rather than the form, so it
  // moves whether or not the form is safe to touch.
  if (cfg.beacon) beaconOffsetHz = cfg.beacon.offset_hz;
  if (formDirty) {
    setSaveStatus(
      "settings changed on the monitor — save to overwrite, or reload to see them",
      "err",
    );
    return;
  }
  applyConfig(cfg);
}

function setSaveStatus(text, cls) {
  const el = $("save-status");
  el.textContent = text;
  el.className = cls || "";
}

async function saveConfig(ev) {
  ev.preventDefault();
  const f = $("settings-form");
  const body = {
    driver: f.driver.value.trim(),
    frequency: parseFloat(f.frequency.value),
    mode: f.mode.value,
    sample_rate: parseFloat(f.sample_rate.value),
    gain: f.gain.value === "" ? null : parseFloat(f.gain.value),
    ppm: parseFloat(f.ppm.value || "0"),
    beacon: {
      offset_hz: parseFloat(f.beacon_offset_hz.value || "0"),
      bandwidth_hz: parseFloat(f.beacon_bandwidth_hz.value || "50"),
    },
    http: { bind: f.http_bind.value.trim() || "0.0.0.0:5760" },
  };
  const enabled = f.mw_enabled.checked;
  const token = f.mw_token.value;
  const beaconId = f.mw_beacon_id.value.trim();
  const gridsquare = f.mw_gridsquare.value.trim();
  if (enabled || token || beaconId || gridsquare) {
    body.microwaveprop = {
      enabled,
      monitor_token: token,
      beacon_id: beaconId,
      gridsquare,
    };
  }
  // Always sent: an omitted block means "keep what is running", which
  // would silently discard whatever the operator just ticked.
  body.update = {
    enabled: f.up_enabled.checked,
    auto: f.up_auto.checked,
    check_interval: Math.max(
      60,
      Math.round(parseFloat(f.up_interval.value || "60") * 60),
    ),
  };
  setSaveStatus("saving…");
  const r = await fetch("/api/config", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (r.ok) {
    setSaveStatus("saved · worker restarting", "ok");
    setTimeout(() => setSaveStatus("", ""), 4000);
    formDirty = false;
    // The daemon broadcasts the config it applied, so this page and every
    // other one open re-render from that frame. Move the marker now so
    // the next waterfall row already uses the new offset.
    beaconOffsetHz = body.beacon.offset_hz;
  } else {
    let msg = "save failed";
    try { msg = (await r.json()).error || msg; } catch (e) {}
    setSaveStatus(msg, "err");
  }
}

function main() {
  initCanvas();
  loadConfig();
  loadUpdate();
  const form = $("settings-form");
  form.addEventListener("submit", saveConfig);
  // Anything typed or ticked makes the form the operator's, not the
  // daemon's, until they save it.
  const markDirty = () => {
    formDirty = true;
  };
  form.addEventListener("input", markDirty);
  form.addEventListener("change", markDirty);
  $("refresh-devices").addEventListener("click", () => loadDevices());
  $("btn-check").addEventListener("click", () => requestUpdate("/api/update/check"));
  $("btn-install").addEventListener("click", () => {
    const target = updateInfo && updateInfo.latest ? buildLabel(updateInfo.latest) : "the new build";
    if (!confirm(`Install ${target}? The daemon restarts itself in place — the SDR reopens and live history resets.`)) {
      return;
    }
    requestUpdate("/api/update/install");
  });
  // A tab hidden while the laptop slept can come back holding a socket
  // that is already gone; check on the way in instead of waiting out the
  // silence timer.
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) return;
    if (!ws || ws.readyState === WebSocket.CLOSED || ws.readyState === WebSocket.CLOSING) {
      dropSocket();
      connectWS();
    } else {
      armSilenceTimer();
    }
  });
  connectWS();
}

main();
