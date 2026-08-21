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

let ws = null;
function connectWS() {
  const url = (location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws";
  ws = new WebSocket(url);
  ws.onopen = () => {
    $("hdr-summary").textContent = "connected";
  };
  ws.onclose = () => {
    $("hdr-summary").textContent = "disconnected — reconnecting…";
    setTimeout(connectWS, 1500);
  };
  ws.onmessage = (msg) => {
    let ev;
    try { ev = JSON.parse(msg.data); } catch (e) { return; }
    switch (ev.type) {
      case "device_info": device = ev; updateHeader(); break;
      case "raw_level":   lastRaw = ev.dbfs; updateHeader(); break;
      case "waterfall":   scrollAndDrawRow(ev.bins, ev.f0_hz, ev.bin_hz); break;
      case "measurement": setReadout(ev); break;
      case "upload":      setUploadStatus(ev); break;
      case "period_started": break;
      case "error":       $("hdr-summary").textContent = "error: " + ev.message; break;
    }
  };
}

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

  // If the saved value isn't in the live list, add it as a disconnected
  // entry so the form still reflects what's on disk.
  if (current && ![...sel.options].some((o) => o.value === current)) {
    const opt = document.createElement("option");
    opt.value = current;
    opt.textContent = `${current} (not connected)`;
    sel.appendChild(opt);
  }
  if (!sel.options.length) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "no SoapySDR devices found";
    opt.disabled = true;
    sel.appendChild(opt);
  }
  sel.value = current;
}

async function loadConfig() {
  const r = await fetch("/api/config");
  if (!r.ok) return;
  const cfg = await r.json();
  await loadDevices(cfg.driver || "");
  $("settings-form").frequency.value = cfg.frequency || "";
  $("settings-form").mode.value = cfg.mode || "beacon";
  $("settings-form").sample_rate.value = cfg.sample_rate || 250000;
  $("settings-form").gain.value = cfg.gain != null ? cfg.gain : "";
  $("settings-form").ppm.value = cfg.ppm || 0;
  // period_seconds is intentionally not in the form — it's a rarely-changed
  // value (the upload cadence the microwaveprop rate-limit assumes). Edit
  // config.yaml directly to override the 60 s default.
  if (cfg.beacon) {
    $("settings-form").beacon_offset_hz.value = cfg.beacon.offset_hz;
    $("settings-form").beacon_bandwidth_hz.value = cfg.beacon.bandwidth_hz;
    beaconOffsetHz = cfg.beacon.offset_hz;
  }
  $("settings-form").http_bind.value = (cfg.http && cfg.http.bind) || "0.0.0.0:5760";
  if (cfg.microwaveprop) {
    $("settings-form").mw_enabled.checked = cfg.microwaveprop.enabled !== false;
    $("settings-form").mw_token.value = cfg.microwaveprop.monitor_token || "";
    $("settings-form").mw_beacon_id.value = cfg.microwaveprop.beacon_id || "";
    $("settings-form").mw_gridsquare.value = cfg.microwaveprop.gridsquare || "";
  } else {
    $("settings-form").mw_enabled.checked = false;
  }
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
  setSaveStatus("saving…");
  const r = await fetch("/api/config", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (r.ok) {
    setSaveStatus("saved · worker restarting", "ok");
    setTimeout(() => setSaveStatus("", ""), 4000);
    // Update beacon marker immediately so the next waterfall row reflects
    // the new offset without needing to reload.
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
  $("settings-form").addEventListener("submit", saveConfig);
  $("refresh-devices").addEventListener("click", () => loadDevices());
  connectWS();
}

main();
