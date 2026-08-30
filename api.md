# propmonitor API

This document is the integration contract for `propmonitor`. It covers six
surfaces:

1. The HTTP REST API served on the operator's LAN.
2. The WebSocket stream the browser UI consumes.
3. The on-disk `config.yaml` format.
4. The outbound HTTP POST contract to **microwaveprop** for uploading
   beacon-signal-level measurements.
5. The two-way config-sync protocol **microwaveprop** drives over a
   WebSocket for managed monitors.
6. The self-update channel the daemon polls for new builds.

Anyone integrating with propmonitor — extending the web UI, writing a
third-party dashboard, or working on the microwaveprop side of §4/§5 —
should be able to do so from this file alone.

Sections 1–3 and 6 are propmonitor's own surfaces and this file is
authoritative for them. Sections 4 and 5 describe endpoints microwaveprop
serves; both are
implemented and live there, and `docs/api/openapi.yaml` in that repo is the
machine-readable spec. When the two disagree, the Elixir side wins and this
file is the thing that needs fixing.

---

## 1. HTTP REST API

Bind: `0.0.0.0:5760` by default (configurable via `http.bind`). LAN-accessible,
no authentication. Intended for trusted networks.

All responses are `application/json` unless noted. Errors raised by a handler
use the shape `{"error": "<message>"}` with an appropriate `4xx` / `5xx`
status. A request body that axum's JSON extractor cannot deserialize at all is
rejected before the handler runs, with the extractor's own plain-text `4xx` —
so don't assume every error carries that JSON shape.

### `GET /`

Returns the embedded `index.html` for the single-page web UI.
Response: `200 text/html`.

### `GET /assets/app.js`, `GET /assets/style.css`

Returns the embedded static file. Response: `200` with the appropriate
`Content-Type`. These are the only two asset routes; anything else under
`/assets/` is an unrouted `404`.

### `GET /api/config`

Returns the currently active configuration as JSON. This is the same value as
`config.yaml` but in JSON form.

Example:

```json
{
  "frequency": 28330000,
  "mode": "beacon",
  "driver": "rtlsdr,serial=03340219",
  "sample_rate": 250000,
  "gain": 10,
  "ppm": 0,
  "period_seconds": 60,
  "beacon": {
    "offset_hz": 0,
    "bandwidth_hz": 50
  },
  "http": {
    "bind": "0.0.0.0:5760"
  },
  "microwaveprop": {
    "enabled": true,
    "monitor_token": "redacted",
    "beacon_id": "00000000-0000-0000-0000-000000000000",
    "gridsquare": "EM12il"
  },
  "update": {
    "enabled": true,
    "auto": true,
    "check_interval": 3600
  }
}
```

The `monitor_token` field is **redacted** on read — the response contains the
literal string `"redacted"` if a token is configured, or the whole
`microwaveprop` object is `null` when the block is absent from `config.yaml`.
The real token is only ever written via `PUT /api/config`.

`microwaveprop.config_version` (§3, §5) is deliberately **not** in this view.
It is bookkeeping between the node and microwaveprop, not something the LAN
UI sets, and `PUT /api/config` carries the persisted value forward untouched.

The `update` object is always present: an absent `update:` block in
`config.yaml` means "every default", not "off". See §6.

### `PUT /api/config`

Replaces the active configuration. The request body must be a complete config
object in the same shape as `GET /api/config` returns. To leave the
`monitor_token` unchanged, send `"redacted"`; any other value (including
`null`) is treated as a real update.

Behavior:

1. Validate the body. On failure: `400 Bad Request` with an `error` message.
2. Atomically rewrite `config.yaml` (write to `config.yaml.tmp`, fsync,
   rename). A failed write or a failed rebind is `500 Internal Server Error`.
3. Signal the worker to stop, wait for it, spawn a new worker with the new
   config. The SoapySDR device is released and reopened.
4. Wake the sync task so the edit is reported up to microwaveprop as a
   `config_report` (§5). Nothing about the response depends on that.
5. Respond `200 OK` with the new config (token still redacted).

During step 3 the WebSocket stream pauses briefly. The new worker emits a
fresh `device_info` event when it comes up, which clients use to detect the
transition.

Within the `microwaveprop` object, `enabled` defaults to `true` and
`gridsquare` defaults to `""` when omitted; `monitor_token` and `beacon_id`
are required whenever the object is present.

The `update` object may be omitted entirely, in which case the node's
current self-update settings carry forward unchanged — that is exactly what
a config push from microwaveprop does (§5 never carries `update`). The web
UI always sends it. Within the object, every field is optional and defaults
to `enabled: true`, `auto: true`, `check_interval: 3600`.

### `GET /api/update`

Returns the state of the self-update channel (§6).

```json
{
  "enabled": true,
  "auto": true,
  "check_interval": 3600,
  "current": {
    "version": "0.2.0",
    "commit": "0f1e2d3c4b5a69788796a5b4c3d2e1f001234567",
    "dist_build": true,
    "can_self_update": true
  },
  "latest": {
    "version": "0.2.0",
    "commit": "9a8b7c6d5e4f30211029384756abcdef01234567",
    "built_at": "2026-08-30T18:04:11Z",
    "assets": {
      "propmonitor-x86_64-linux": "e3b0c442…",
      "propmonitor-aarch64-linux": "9f86d081…",
      "propmonitor-armv7-linux": "2c26b46b…"
    }
  },
  "phase": "idle",
  "last_check_at": "2026-08-30T18:12:00Z",
  "last_error": null,
  "install_path": "/opt/propmonitor/bin/propmonitor"
}
```

| Field | Meaning |
|---|---|
| `enabled`, `auto`, `check_interval` | the `update:` block from `config.yaml` (§3) |
| `current.version` | crate version of the running build |
| `current.commit` | commit it was built from; `"dev"` for a local build |
| `current.dist_build` | built by the release workflows. Unattended updates require it |
| `current.can_self_update` | in-place activation is available on this platform (Linux) |
| `latest` | the build the channel offered on the last check, or `null` when up to date / never checked |
| `phase` | `idle`, `checking`, `downloading`, `installing` |
| `last_check_at` | RFC 3339 time of the last completed check, successful or not |
| `last_error` | why the last check or install failed; `null` after a success |
| `install_path` | where a new binary lands — the running executable's own path |

`latest` being `null` with `last_check_at` set means "up to date". Both
`null` means no check has completed yet.

### `POST /api/update/check`

Asks the node to check the channel now. Returns `200` with the same body as
`GET /api/update`, captured before the check runs — the check itself is
asynchronous and reports through the `update` WebSocket event (§2) and
subsequent `GET /api/update` calls.

Works regardless of `update.enabled`: that flag governs the periodic timer,
not an explicit operator request.

### `POST /api/update/install`

Installs the build the last check found. Returns `200` with the
`GET /api/update` body once the install has been started, or `409 Conflict`
with `{"error": …}` when it cannot be:

| Condition | Message |
|---|---|
| a check or install is already running | `an update check or install is already running` |
| platform has no in-place activation | `in-place self-update requires a Linux install` |
| nothing found by the last check | `no update available — check for updates first` |

On success the daemon downloads, verifies and activates the new binary, then
replaces its own process image. Connections drop as they would across any
restart; the PID does not change. See §6 for the full sequence.

### `GET /api/devices`

Enumerates every SoapySDR device the host currently sees. Used by the
web UI to populate the **SDR device** dropdown in the settings form.

```json
{
  "devices": [
    {
      "value": "rtlsdr,serial=03340219",
      "label": "Nooelec NESDR SMArt v5 (03340219)"
    },
    {
      "value": "sdrplay,serial=210504BB98",
      "label": "SDRplay RSP1A (210504BB98)"
    }
  ]
}
```

`value` is what goes into the `driver` field of `PUT /api/config` and
ends up as the `driver:` string in `config.yaml`. The list reflects
plug state at the moment of the call — replug a dongle then refresh
the dropdown.

Running this endpoint can take ~100–500 ms because SoapySDR scans USB
synchronously; the server runs the call on a blocking thread so other
requests aren't affected.

### `GET /api/status`

Returns a snapshot of the runtime state.

```json
{
  "device": {
    "actual_sample_rate": 250000,
    "actual_frequency": 28330000,
    "actual_gain": 10.0,
    "gain_elements": ["TUNER"]
  },
  "last_raw_dbfs": -34.2,
  "last_measurement": {
    "measured_at": "2026-05-13T15:30:00Z",
    "noise_floor_dbfs": -110.2,
    "signal_peak_dbfs": -88.4,
    "signal_avg_dbfs": -89.1,
    "snr_peak_db": 21.8,
    "snr_avg_db": 21.1,
    "signal_active_fraction": 0.48
  },
  "uploader": {
    "enabled": true,
    "last_post_at": "2026-05-13T15:30:01Z",
    "last_status": "ok",
    "queued": 0
  }
}
```

Any field can be `null` if the corresponding event hasn't fired yet (e.g. on
fresh start, before the first period completes).

### `GET /api/measurements?limit=N`

Returns the most recent `N` measurements from the in-memory ring buffer.
Default `limit=100`. Max `limit=1440` (24h at one-per-minute).

Response:

```json
{
  "measurements": [
    {
      "measured_at": "2026-05-13T15:30:00Z",
      "noise_floor_dbfs": -110.2,
      "signal_peak_dbfs": -88.4,
      "signal_avg_dbfs": -89.1,
      "snr_peak_db": 21.8,
      "snr_avg_db": 21.1,
      "signal_active_fraction": 0.48
    },
    …
  ]
}
```

Measurements are not persisted to disk — only the last 24 hours are retained
in memory. If propmonitor restarts, the local history resets. The microwaveprop
side is the long-term store.

### `GET /ws`

WebSocket upgrade. See §2 for the frame schema.

---

## 2. WebSocket stream

Connect to `/ws`. All frames are **JSON text frames**, one event per frame,
tagged by `"type"`. The server pushes; the client never sends application
frames.

### Event types

#### `device_info` — emitted once when the worker comes up, and replayed to every new client

A client that connects mid-run immediately receives the last `device_info`
rather than waiting for the next worker restart, so headers render on the
first frame.

```json
{
  "type": "device_info",
  "actual_sample_rate": 250000,
  "actual_frequency": 28330000,
  "actual_gain": 10.0,
  "gain_elements": ["TUNER"]
}
```

#### `raw_level` — ~5 Hz, raw IQ RMS in dBFS

```json
{ "type": "raw_level", "dbfs": -34.2 }
```

#### `waterfall` — ~10 Hz, one row of FFT bins

```json
{
  "type": "waterfall",
  "f0_hz": -125000.0,
  "bin_hz": 244.140625,
  "bins": [0.00012, 0.00018, ...]
}
```

`bins.length == 1024` (`worker::WATERFALL_FFT_N`). This is the display FFT
only — the measurement path in `measure.rs` runs its own 16384-point FFT, so
"frame" in §4's gating discussion means a 16384-sample analysis frame, not a
waterfall row. Values are **linear power** (`|x|²`), not dB. The first
bin corresponds to frequency `f0_hz` relative to the tuned center; bin `k`
corresponds to `f0_hz + k * bin_hz`. The center bin (index 512) is DC. Clients
convert to dB with `10 * log10(max(value, 1e-30))`.

#### `period_started` — emitted at the start of each measurement window

```json
{ "type": "period_started", "at": "2026-05-13T15:30:00Z" }
```

#### `measurement` — emitted at the end of each measurement window

```json
{
  "type": "measurement",
  "measured_at": "2026-05-13T15:30:00Z",
  "noise_floor_dbfs": -110.2,
  "signal_peak_dbfs": -88.4,
  "signal_avg_dbfs": -89.1,
  "snr_peak_db": 21.8,
  "snr_avg_db": 21.1,
  "signal_active_fraction": 0.48
}
```

`signal_peak_dbfs` / `signal_avg_dbfs` / `snr_*_db` are computed only over
frames whose in-band power exceeded `noise_floor + 3 dB` (signal-present
gating). `signal_active_fraction` is the duty cycle of those frames over
the window. See §4 for the same gating in the upload payload.

#### `upload` — emitted after each microwaveprop POST attempt

```json
{
  "type": "upload",
  "at": "2026-05-13T15:30:01Z",
  "status": "ok",
  "queued": 0
}
```

`status` is `"ok"` for `2xx` responses or `"error"` for anything else
(including network failure). On error, `queued` reflects the current retry-
queue depth.

#### `error` — a worker-level failure

```json
{ "type": "error", "message": "stream read failed: …" }
```

Emitted when the SDR worker thread dies (device unplugged, stream error,
bad driver args). The message is the Rust error string, meant for display in
the UI, not for parsing. The HTTP server stays up; the fix is normally a new
`PUT /api/config`.

#### `update` — emitted on every self-update state transition

```json
{
  "type": "update",
  "phase": "downloading",
  "latest": {
    "version": "0.2.0",
    "commit": "9a8b7c6d5e4f30211029384756abcdef01234567",
    "built_at": "2026-08-30T18:04:11Z",
    "assets": { "propmonitor-x86_64-linux": "e3b0c442…" }
  },
  "error": null
}
```

`phase` is `idle`, `checking`, `downloading` or `installing`; `latest` and
`error` mirror `GET /api/update`. Sent whether the transition came from the
node's own timer, this client, or another browser — so a UI that renders
this event stays correct without polling.

A client that joins mid-install gets no replay of earlier phases; fetch
`GET /api/update` on load for the current state, then follow these events.
After an `installing` phase the daemon re-executes: expect the WebSocket to
drop and reconnect onto the new build.

### Reconnection

When `PUT /api/config` triggers a worker restart, the WebSocket stream pauses
briefly but does not close. The client sees a new `device_info` event when
the new worker is ready. If the connection drops for any reason, clients
should reconnect with exponential backoff.

---

## 3. Config file format (`config.yaml`)

YAML, hand-edited or written via `PUT /api/config`. The on-disk format is
authoritative; the server reads it at boot and after every config update.

```yaml
frequency: 28330000             # Hz, tuned center frequency
mode: beacon                     # usb | lsb | am | nfm | wfm | cw | beacon
driver: "rtlsdr,serial=03340219"  # SoapySDR driver args
sample_rate: 250000              # Hz
gain: 10                          # dB, omit for AGC
ppm: 0                            # frequency-trim PPM (0 for TCXO devices)
period_seconds: 60                # measurement integration window

# Required only when mode == beacon. Defines the narrow passband for power
# measurement, centered offset_hz away from the tuned frequency.
beacon:
  offset_hz: 0                    # passband center, relative to `frequency`
  bandwidth_hz: 50                # full passband width

http:
  bind: "0.0.0.0:5760"            # address:port

# Optional. The ingest URL itself is hardcoded in src/uploader.rs as
# MICROWAVEPROP_ENDPOINT — every install reports to the same server, only
# the per-station credentials vary. Default: https://prop.w5isp.com/api/v1/…
#
# Set `enabled: false` to pause uploads while keeping credentials saved.
# The "Send beacon reports" checkbox in the web UI maps to this flag.
microwaveprop:
  enabled: true                   # checkbox in the web UI
  monitor_token: "…"              # 32-byte base64-url, from POST /me/beacon-monitors
  beacon_id: "…"                  # UUID of the beacon being monitored
  gridsquare: "EM12il"            # Maidenhead grid of THIS receiver, 4–20 chars
  config_version: 0               # managed monitors only — see §5

# Optional; every field defaults. Absent block == all defaults, i.e. the
# self-update channel is ON. Node-local: microwaveprop never sets these
# and a config push from the website carries them forward untouched (§6).
update:
  enabled: true                   # poll the release channel on a timer
  auto: true                      # install new builds without being asked
  check_interval: 3600            # seconds between checks, minimum 60
```

Validation rules:

- `frequency` finite and > 0.
- `mode` must be one of the listed values.
- `sample_rate` finite and >= 250000 (lower rates pin the waterfall FFT under
  250 Hz/bin, unacceptable for visual band overview).
- `gain` either a finite number or omitted.
- `ppm` finite.
- `period_seconds` >= 5.
- `mode: beacon` requires a `beacon:` block.
- `beacon.offset_hz` finite; `beacon.bandwidth_hz` finite, > 0, and
  <= `sample_rate / 2`.
- The whole passband must fit inside the sampled spectrum:
  `|offset_hz| + bandwidth_hz / 2 <= sample_rate / 2`.
- `microwaveprop.gridsquare` is either empty or 4–20 characters.
- `microwaveprop.enabled` must be `true` AND `monitor_token` must be non-empty AND `beacon_id` must be non-empty AND `gridsquare` must be non-empty for uploads to actually run. Any of these conditions false → uploads paused.
- `microwaveprop.config_version` must be a non-negative integer. Defaults to
  `0`. Written by propmonitor itself, never by hand: it is the last version
  microwaveprop handed this node (§5). Editing or dropping it costs one
  spurious config re-push and SDR restart.
- `update.enabled` / `update.auto` must be `true` or `false`.
- `update.check_interval` is an integer >= 60 seconds. The floor keeps a
  fleet of nodes off the release endpoint regardless of what the UI sends.
- `update.auto` only takes effect on an official (CI-built) binary running
  on Linux; see §6.

The in-tree YAML parser (`src/yaml.rs`) supports the subset used here:
scalars (numbers, quoted/unquoted strings, comments) and one level of nesting
indented two spaces. Lists are not used. Don't use anchors, aliases, or
multi-document syntax.

---

## 4. microwaveprop upload contract

This is the wire format the propmonitor uploader emits. The matching server
endpoint is **live** in microwaveprop
(`MicrowavepropWeb.Api.V1.BeaconMonitorMeasurementController`), and the
authoritative machine-readable spec is `docs/api/openapi.yaml` in that repo.

### Endpoint

The full URL is hardcoded in propmonitor at `src/uploader.rs` —
`MICROWAVEPROP_ENDPOINT`. The default is:

```
POST https://prop.w5isp.com/api/v1/beacon-monitor/measurements
Authorization: Bearer <monitor_token>
Content-Type: application/json
```

`<monitor_token>` is the 32-byte base64-url string returned by
`POST /api/v1/me/beacon-monitors` on the microwaveprop side. It authenticates
a *monitor*, not a user, and is resolved by a dedicated plug
(`MicrowavepropWeb.Api.MonitorAuth`) rather than the user-API-token plug —
the two token spaces are separate and not interchangeable. A monitor token is
also readable any time from the monitor's page on the website, and rotating
one is a deliberate act there; a rotated token invalidates the old one
immediately.

Monitors come in two `kind`s. `self_service` is the Python client; `managed`
is this Rust daemon plus the two-way config sync in §5. Both upload
measurements through this endpoint identically — `kind` only gates §5.

### Request body

```json
{
  "beacon_id":              "00000000-0000-0000-0000-000000000000",
  "gridsquare":             "FN31pr",
  "frequency_hz":           28330000,
  "measured_at":            "2026-05-13T15:30:00Z",
  "integration_s":          60,
  "passband_hz":            300,
  "gain_db":                10.0,
  "noise_floor_dbfs":       -110.2,
  "signal_peak_dbfs":       -88.4,
  "signal_avg_dbfs":        -89.1,
  "snr_peak_db":            21.8,
  "snr_avg_db":             21.1,
  "signal_active_fraction": 0.48,
  "propmonitor_version":    "0.2.0"
}
```

Field semantics:

| Field | Type | Notes |
|---|---|---|
| `beacon_id` | string (UUID) | Identifies *which* beacon this measurement is for. Canonical key on the microwaveprop side. A monitor record is not bound to one beacon: any monitor may report for any beacon that is **approved** and **on the air**, and anything else is a `404`. |
| `gridsquare` | string | Maidenhead grid square of the RECEIVER station (e.g. `"FN31pr"`). The server caps it at 20 characters; propmonitor additionally requires 4–20 before it will upload at all. Lets the server correlate signal strength with propagation-path distance and bearing. |
| `frequency_hz` | integer | The propmonitor *tuned* frequency in Hz. Not the beacon's nominal frequency; the operator may intentionally tune slightly off to compensate for radio offset. |
| `measured_at` | string | UTC ISO-8601 timestamp at the **start** of the integration window. |
| `integration_s` | number | The measurement window length in seconds. Equal to `period_seconds` in propmonitor config. |
| `passband_hz` | number | The bandwidth of the power-measurement window in Hz. Operator picks this wide enough to cover whichever modes the beacon uses (e.g. ~300 Hz for a beacon that interleaves CW and Q65). |
| `gain_db` | number | SDR gain actually reported by the device at upload time. Lets the server detect operator gain changes and rebaseline dBFS trends. With AGC on, this is whatever the tuner settled on. |
| `noise_floor_dbfs` | number | Median of out-of-passband bin power, in dBFS. |
| `signal_peak_dbfs` | number | Peak in-passband power across detected-on-air frames, in dBFS. |
| `signal_avg_dbfs` | number | Mean in-passband power across detected-on-air frames, in dBFS. Mode-invariant — see gating note below. |
| `snr_peak_db` | number | `signal_peak_dbfs - noise_floor_dbfs`. |
| `snr_avg_db` | number | `signal_avg_dbfs - noise_floor_dbfs`. |
| `signal_active_fraction` | number | Fraction of FFT frames in the window where in-band power exceeded `noise_floor + 3 dB`. `1.0` = continuous carrier; `~0.5` = 50%-keyed CW; `0.0` = nothing heard. |
| `propmonitor_version` | string | Build version of the reporting client. Diagnostic only — helps the server correlate stat shifts with client upgrades. Max 32 characters. |

Everything above except `gridsquare` is **required** by the server; a missing
field is a `422`. `gridsquare` is optional server-side (the Python client may
omit it) but propmonitor refuses to upload without one — see §3.

The server also accepts five optional fields this client never sends, used by
the self-service Python client: `op_mode` (≤32 chars), `decoded_callsign`
(≤20), `decoded_grid` (≤8), `decoded_dbm` (−160…60), and
`frequency_offset_hz`. Sending them is legal; omitting them is normal.

### Server-side value bounds

Out-of-range values are rejected as `422` rather than clamped, so a client bug
is loud instead of silently poisoning the stats:

| Field | Accepted range |
|---|---|
| `frequency_hz` | > 0 |
| `integration_s` | 5 … 3600 |
| `passband_hz` | > 0 |
| `gain_db` | −10 … 80 |
| `noise_floor_dbfs`, `signal_peak_dbfs`, `signal_avg_dbfs` | −200 … 0 |
| `snr_peak_db`, `snr_avg_db` | −50 … 120 |
| `signal_active_fraction` | 0.0 … 1.0 |
| `measured_at` | within ±24 h of server time |

The `measured_at` window exists so a monitor cannot back- or forward-date
measurements into hourly aggregates. A node with a badly wrong clock will see
every upload rejected with `422`; check NTP before checking anything else.

### Signal-present gating

The peak/avg/SNR fields are computed **only** over frames whose in-band
power exceeded `noise_floor + 3 dB` (a ×2 linear multiplier against the
median per-bin noise estimate). This makes the signal stats
mode-invariant: a 30%-keyed CW beacon and a continuous Q65 transmission
with the same TX power produce the same `signal_avg_dbfs`. The duty
cycle shows up in `signal_active_fraction` instead. The 3 dB threshold
is hardcoded in v1.

If no frames cross the threshold, `signal_active_fraction == 0.0` and
the peak/avg fall back to ungated values (which sit near the noise
floor; SNR ≈ 0 dB). The server should treat that case as "no signal
heard" and either filter it out of trend lines or carry it explicitly.

### Calibration

`dBFS` is uncalibrated — it's relative to the SDR ADC full scale and
depends on gain. To convert to absolute power (dBm) the microwaveprop
side may one day keep a per-monitor calibration offset (possibly indexed by
`gain_db`); no such offset exists today, and it would live entirely on the
server — nothing about it is configured in propmonitor or carried in the
upload. Until then, `snr_*_db` is the gain-independent field and the one
downstream analysis should prefer.

### Responses

Errors are RFC 9457 `application/problem+json`, not the `{"error": …}` shape
the LAN API uses:

```json
{
  "type": "about:blank",
  "title": "not_found",
  "status": 404,
  "detail": "beacon_id is unknown, unapproved, or marked off-the-air."
}
```

A `422` adds an `errors` object mapping each rejected field to its messages:

```json
{
  "type": "about:blank",
  "title": "validation_failed",
  "status": 422,
  "detail": "One or more fields are invalid.",
  "errors": { "gain_db": ["must be less than or equal to 80.0"] }
}
```

`title` is a stable slug; `detail` is prose and may be reworded. propmonitor
dispatches on the status code alone and never parses the body — the shape is
documented for humans debugging a station.

| Status | Meaning | Client behavior |
|---|---|---|
| `204 No Content` | Accepted and recorded. The monitor's `last_seen_at` is stamped — an accepted upload *is* the self-service heartbeat. | Pop from queue, reset backoff. |
| `401 Unauthorized` | Bad/missing/revoked/expired monitor token. | Drop the measurement. Uploads keep failing until the token is fixed. |
| `404 Not Found` | `beacon_id` is unknown, not a UUID, unapproved, or marked off-the-air. | Drop the measurement. Retrying will not help; an admin un-approving a beacon is meant to stop the monitor cleanly. |
| `409 Conflict` | A measurement for this `(monitor, beacon, measured_at)` already exists — normally a retried POST whose first attempt actually landed. | Drop the measurement. Nothing is lost. |
| `422 Unprocessable Entity` | Malformed body, missing required field, or an out-of-range value. | Drop the measurement (a schema mismatch won't fix itself). |
| `429 Too Many Requests` | Rate limited. | Keep in queue, retry with exponential backoff (1 s → 5 min cap). |
| `5xx` | Server-side failure. | Keep in queue, retry with exponential backoff (1 s → 5 min cap). |
| network error / timeout | Unreachable. | Same as `5xx`. |

The client's actual rule (`uploader::classify_status`) is coarser than the
table: **2xx → accepted, 429 → retry, any other 4xx → drop, everything else
→ retry.** The per-status rows above describe why each code lands where it
does; a new 4xx added server-side will be dropped by existing clients, so
anything meant to be retried must be a 429 or a 5xx.

### Retry queue

The propmonitor uploader holds a bounded in-memory queue (default cap: 1440
entries = 24 h of one-per-minute measurements). When the queue is full,
oldest entries are dropped. The queue is not persisted across propmonitor
restarts.

### Rate / batching

One measurement per POST; payloads are never batched. If
`period_seconds == 60`, the steady-state rate is 1/minute per monitor.

Microwaveprop rate-limits **per monitor token** (not per IP, so co-located
stations behind one NAT don't punish each other) at **60 requests per 60 s
fixed window**, shared across the ingest endpoint and the §5 polling
endpoints. That is ~60× the steady-state rate, leaving headroom for a
queue-drain burst after an outage. Responses carry `ratelimit-limit`,
`ratelimit-remaining`, and `ratelimit-reset`; a `429` also carries
`retry-after` (seconds). propmonitor's own backoff already respects the
spirit of these headers without reading them.

---

## 5. microwaveprop config-sync protocol (managed monitors)

A **managed** monitor is created on the microwaveprop website, which then
holds the authoritative config plus a version counter and drives the node
over a WebSocket. An install with no `monitor_token` never touches any of
this — the sync task idles until a token is configured, and starts without a
restart when one is pasted into the LAN UI.

Kind matters here in a way it does not for §4: a token minted for a
`self_service` monitor authenticates uploads perfectly well but is refused
`403` on every endpoint in this section. If a propmonitor node logs repeated
`403`s from sync, the monitor was created with the wrong kind on the website,
and kind is immutable once created — make a new monitor.

Implemented in `src/sync.rs`. The matching server side is
`MicrowavepropWeb.MonitorSocket` (frames) and
`MicrowavepropWeb.Api.V1.BeaconMonitorConfigController` (polling fallback) on
the microwaveprop repo.

### Endpoint

```
GET wss://prop.w5isp.com/api/v1/beacon-monitor/socket
Authorization: Bearer <monitor_token>
```

Same `monitor_token` as §4, in a header rather than a query string so it
stays out of proxy access logs. The URL is hardcoded as
`MICROWAVEPROP_SYNC_ENDPOINT` in `src/sync.rs`.

Auth rides the HTTP upgrade, so there is no in-band authentication frame. The
upgrade is refused with:

- `401` — bad, missing, revoked, or expired token.
- `403` — the token belongs to a `self_service` monitor. Only `managed`
  monitors have a server-side config to sync, and the same `403` applies to
  every polling endpoint below.
- `426` — the request arrived without WebSocket upgrade headers.

Only one socket per monitor is served: when a reconnect lands on a different
pod, the server displaces the older session rather than running two. A node
that sees its socket closed for no apparent reason should simply reconnect.

The server's idle timeout is 120 s, which the node's 30 s ping and 60 s
`status` frame stay well inside.

### Envelope

Every frame is a JSON **text** frame carrying `v` (protocol version, `1`)
and `type`. Remaining keys are frame-specific and sit at the top level, not
nested in a `payload` object. Unknown keys are ignored on both sides. An
unknown `type` is logged and skipped by the node, and answered with an
`unknown_type` error frame by microwaveprop; neither drops the session.
Binary frames are rejected (`bad_frame`) — everything here is text.

### The synced config object

```json
{
  "frequency": 28330000,
  "mode": "beacon",
  "driver": "rtlsdr,serial=03340219",
  "sample_rate": 250000,
  "gain": 10,
  "ppm": 0,
  "period_seconds": 60,
  "beacon": { "offset_hz": 0, "bandwidth_hz": 50 },
  "upload_enabled": true,
  "beacon_id": "00000000-0000-0000-0000-000000000001",
  "gridsquare": "EM12il"
}
```

This is **not** the `GET /api/config` shape. Two deliberate omissions:

- **`monitor_token`** — never appears in a sync payload in either
  direction. The node's wire struct (`SyncConfig`) has no field for it, so
  a leak isn't possible by construction. On apply, the node reuses the
  `"redacted"` sentinel path from `PUT /api/config` to keep its local token.
- **`http`** — pushing a bad `bind` from the website would strand the LAN
  UI with no remote way back in. The node keeps its own `http` block.

`upload_enabled` maps to `microwaveprop.enabled` on disk. Microwaveprop
mirrors §3's validation in `Microwaveprop.BeaconMonitors.ManagedConfig` so it
never pushes a config the node will refuse, and adds three bounds of its own
that the node does not enforce: `period_seconds <= 86400`, `driver` 1–200
characters, and `beacon_id` must parse as a UUID (or be empty). A config that
fails validation is answered with an `error` frame (`invalid_config`) or a
`422`, and the stored config is left alone.

On the node, `gain`, `ppm`, `beacon`, `upload_enabled`, `beacon_id` and
`gridsquare` are optional on the way in and fall back to
`null`/`0`/`null`/`true`/`""`/`""`. A missing required field (`frequency`,
`mode`, `driver`, `sample_rate`, `period_seconds`) makes the frame
unparsable, which costs the node the ability to answer — send them all.

### Frames

| Frame | Direction | Keys |
|---|---|---|
| `hello` | node→prop | `client_version`, `applied_config_version`, `local_ip`, `local_port`, `config` |
| `hello_ack` | prop→node | `config_version` |
| `config_push` | prop→node | `version`, `config` |
| `config_applied` | node→prop | `version`, `ok`, `error` |
| `config_report` | node→prop | `base_version`, `config` |
| `config_accepted` | prop→node | `version` |
| `status` | node→prop | `local_ip`, `local_port`, `uploader`, `last_measurement` (microwaveprop also accepts an optional `client_version` here; propmonitor sends it only in `hello`) |
| `error` | prop→node | `code`, `message` |

#### `hello` — first frame on every connection

```json
{
  "v": 1,
  "type": "hello",
  "client_version": "0.2.0",
  "applied_config_version": 7,
  "local_ip": "192.168.1.50",
  "local_port": 5760,
  "config": { "…": "as above" }
}
```

`local_ip` is the node's LAN address, detected by `connect`ing a UDP socket
towards `prop.w5isp.com:443` (falling back to the literal `8.8.8.8:80` when
DNS is broken) and reading back `local_addr()`. No packet is sent; the
kernel just reveals the default-route interface. `null` when detection
fails. `local_port` comes from `http.bind`. Together they give the website
the `http://<local_ip>:<local_port>` link to the node UI — reachable only
from the operator's own network.

Microwaveprop answers `hello_ack {config_version}` and then reconciles, in
one of three ways:

- **`config_version == 0`** (a managed monitor created on the website but
  never configured) — it adopts the node's `config`, mints version 1, and
  follows the ack with `config_accepted {version}`. If the `hello` carried no
  usable `config` there is nothing to adopt and nothing is sent.
- **`applied_config_version < config_version`** — it follows the ack with
  `config_push {version, config}`.
- **otherwise** — in sync; the ack is the whole answer.

The `hello` also stamps the node's `local_ip`, `local_port`, and
`client_version` on the monitor record, and marks it connected.

#### `config_push` / `config_applied`

```json
{ "v": 1, "type": "config_push", "version": 9, "config": { "…": "…" } }
{ "v": 1, "type": "config_applied", "version": 9, "ok": true, "error": null }
```

Node behavior, in order:

1. `version <= applied_config_version` → **ignored**, no reply. This is what
   makes a reboot cheap and an echoed report a no-op.
2. Content equal to the running config → persist the version, **skip the
   apply**, reply `ok: true`. A no-op push must never drop and reopen the
   SDR.
3. Otherwise → apply it (same code path as `PUT /api/config`: validate,
   rewrite `config.yaml` atomically, restart the worker), then reply. A
   rejected config replies `ok: false` with `error` set and leaves the
   running config untouched.

Server side, `ok: true` advances the monitor's applied-version marker, guarded
so a replayed or out-of-order frame cannot walk it backwards. `ok: false` is
logged with the `error` string and nothing else happens — the website keeps
showing the node as behind, which is the truth. A `config_applied` without an
integer `version` earns a `bad_frame` error.

#### `config_report` / `config_accepted`

```json
{ "v": 1, "type": "config_report", "base_version": 9, "config": { "…": "…" } }
{ "v": 1, "type": "config_accepted", "version": 10 }
```

Sent after every successful local edit through `PUT /api/config`.
Microwaveprop accepts it unconditionally *if it validates* — last write wins,
arbitrated by arrival order there — mints the next version, records it as
already applied (the node is running it, so no push is echoed back), and the
node persists that version. `base_version` is accepted for symmetry but
ignored: versions are minted only by microwaveprop. An invalid config is
answered with an `error` frame (`invalid_config`) and no version is minted.

If the socket is down the edit stays pending and goes up through the polling
`POST` instead — and if it is still pending when a session drops, the next
session re-sends it.

#### `status` — every 60 s, also the app-level heartbeat

```json
{
  "v": 1,
  "type": "status",
  "local_ip": "192.168.1.50",
  "local_port": 5760,
  "uploader": { "enabled": true, "last_status": "ok", "queued": 0 },
  "last_measurement": {
    "measured_at": "2026-05-13T15:30:00Z",
    "noise_floor_dbfs": -110.2,
    "signal_peak_dbfs": -88.4,
    "signal_avg_dbfs": -89.1,
    "snr_peak_db": 21.8,
    "snr_avg_db": 21.1,
    "signal_active_fraction": 0.48
  }
}
```

`last_measurement` is the `GET /api/status` shape, or `null` before the
first window completes. One status frame goes out immediately after
`hello`, so the website flips to "online" without waiting a minute.

Microwaveprop keeps only the connection facts — `local_ip`, `local_port`,
`client_version`, and the arrival time. `uploader` and `last_measurement` are
accepted for parity with the LAN status shape and then discarded; measurement
data reaches the server through §4, not here. A monitor reads as **online**
for **180 s** after its last status (frame or poll) — two missed heartbeats
plus slack. Nothing stores an "online" flag, so a pod dying with the socket
open cannot strand one.

#### `error`

```json
{ "v": 1, "type": "error", "code": "bad_frame", "message": "…" }
```

Logged by the node; the session continues. Codes:

| Code | Meaning |
|---|---|
| `bad_frame` | Not JSON, missing `v`/`type`, a binary frame, or a frame missing a key its type requires. |
| `unsupported_version` | The frame's `v` is not `1`. |
| `unknown_type` | The server does not know that `type`. |
| `invalid_config` | The `config` in a `hello`, `config_report`, or adoption failed validation. `message` carries the field errors. |

### Versions

**Versions are minted only by microwaveprop.** The node never invents one:
it persists the last version it was handed as `microwaveprop.config_version`
in `config.yaml` (§3) and echoes it in `hello` and `config_report`.
Persisting it means a reboot doesn't look like a node that never applied
its config, so nothing gets re-pushed and the SDR isn't restarted for
nothing. Concurrent web and node edits are last-write-wins, arbitrated by
arrival order at microwaveprop.

### Keepalive and reconnect

The node sends a WebSocket protocol ping every 30 s; the 60 s `status`
frame doubles as an application heartbeat. Both are well under
Cloudflare's ~100 s idle timeout. Reconnect backoff runs 1 s → 5 min.

### Polling fallback

Used **only while the socket is down**, one cycle per 60 s, same Bearer
token:

| Request | Response |
|---|---|
| `GET /api/v1/beacon-monitor/config?known_version=N` | `200 {"version": M, "config": {…}}`, or `304` when `M == N`. `400` if `known_version` is not a non-negative integer; omitting it means `0`. |
| `POST /api/v1/beacon-monitor/config` `{"base_version": N, "config": {…}}` | `200 {"version": M}`. `422` if the body has no `config` object or the config fails validation. |
| `POST /api/v1/beacon-monitor/status` (the `status` frame minus `v`/`type`) | `204` |

All three also answer `401` (bad token), `403` (self-service monitor), and
`429` (the shared per-monitor bucket from §4).

A cycle pushes a pending local edit up **first**, then pulls, then posts
status: reports are accepted unconditionally, so reporting first keeps a
local edit from being clobbered by the version being pulled. The pulled
config goes through the same three-step apply decision as `config_push`.

A monitor still at version 0 has no stored config, and `GET` answers `304`
for the node's implicit `known_version=0` — so the pull never has to render a
null `config`, and the node never has to parse one.

---

## 6. Self-update channel

Every push to `main` rebuilds all three Linux targets and republishes one
fixed GitHub release, tagged `latest`
(`.github/workflows/latest.yml`). Nodes poll it and install what they find.
The daemon side is `src/update.rs`.

### What the channel publishes

| Asset | Consumer |
|---|---|
| `propmonitor-x86_64-linux`, `propmonitor-aarch64-linux`, `propmonitor-armv7-linux` | the daemon, self-updating |
| the matching `.tar.gz` of each | `install.sh` |
| `propmonitor-manifest.json` | the daemon, to decide whether to bother |

One release, updated in place, always named `latest` — so every URL is
stable and a node needs no discovery step:

```
https://github.com/North-Texas-Microwave-Society/propmonitor/releases/download/latest/propmonitor-manifest.json
```

```json
{
  "version": "0.2.0",
  "commit": "9a8b7c6d5e4f30211029384756abcdef01234567",
  "built_at": "2026-08-30T18:04:11Z",
  "assets": {
    "propmonitor-x86_64-linux": "<sha256 hex>",
    "propmonitor-aarch64-linux": "<sha256 hex>",
    "propmonitor-armv7-linux": "<sha256 hex>"
  }
}
```

Binaries are resolved **relative to the manifest URL** — same release, same
directory — so a fork that publishes its own channel is one setting, not
three. `PROPMONITOR_MANIFEST_URL` overrides the manifest URL for a staging
channel or a local test; unset on a normal install.

The `latest` git tag is cosmetic — it exists so the release page shows real
contents. Assets are addressed by the release's tag name, not by the tag
ref, and the publish job verifies the manifest URL is serving the new
commit before it passes. A node that gets a 404 or an unreadable manifest
records it in `last_error` and retries on the next tick; nothing is
installed on a failed check.

### Identity is the commit, not the version

`main` moves without a version bump, so `0.2.0` cannot answer "am I
current?". The comparison is `manifest.commit != current.commit`, where
`current.commit` is baked in at build time by `build.rs` from
`PROP_BUILD_COMMIT` (set by both release workflows). A local `cargo build`
falls back to `git rev-parse HEAD`, then to `"dev"`.

`build.rs` also records whether that environment variable was set at all,
which is what `current.dist_build` reports: **only official builds update
unattended.** A developer's laptop build is never silently replaced by a
release binary — the operator can still press Install.

### Install sequence

All file work happens in the running binary's own directory, so the final
step is an atomic `rename(2)` within one filesystem:

1. **Already installed?** Hash the file at the install path. If it is
   already the published build, the previous cycle installed it and only
   failed to activate it — skip to step 6. Re-installing would rename that
   same build to `propmonitor.prev`, overwriting the one generation of
   backup with the build being installed.
2. Download the asset for this architecture to
   `propmonitor.new.<pid>`, hashing the stream as it lands.
3. Compare SHA-256 against the manifest. A mismatch aborts here — the
   temp file is deleted and the running binary is untouched.
4. `chmod 0755`, `fsync`.
5. **Preflight:** run the new binary as
   `propmonitor --check-config <the node's own config path>`. That mode
   loads and validates the config, binds no port, opens no SDR, and exits.
   Two failures are fatal and both are caught here: a build that cannot
   start at all (bad glibc, missing symbol), and a build that starts and
   then *rejects this node's config* — tightened validation, a key that
   changed shape. The second is why the probe uses the real config: such a
   build would swap in, `execve` fine, and then die during config load,
   which `Restart=always` flaps forever while `propmonitor.prev` is never
   restored. The probe has a 20 s deadline and is killed and reaped if it
   overruns. Builds older than `--check-config` read it as a config path
   and say so; those fall back to the older probe (a config path that
   cannot exist) so republishing an earlier commit stays installable.
6. Rename the current binary to `propmonitor.prev`, rename the new one into
   place, `fsync` the directory. A failed rename rolls the backup back.
7. `execve(2)` the installed path with argv and environment carried over.

Step 7 is why this is smooth: **the PID does not change.** systemd sees no
stop/start, the unit stays `active (running)`, and `Restart=`/start-limit
accounting is untouched. The visible effect is what any restart does — the
SDR is reopened, the in-memory ring buffer resets, WebSocket clients
reconnect. If `execve` fails, the old image is still running and healthy;
the daemon reports it and asks systemd for a conventional restart.

### Requirements on the node

- **Linux.** In-place activation is the whole mechanism; `can_self_update`
  is `false` elsewhere and both auto and manual installs refuse.
- **A writable binary directory.** `install.sh` installs to
  `/opt/propmonitor/bin`, owned by the `propmonitor` service user, and the
  unit lists it in `ReadWritePaths=`. Replacing a binary under
  `/usr/local/bin` as a non-root daemon is not possible, which is why the
  install location moved.

### Rollback

The previous binary is kept next to the current one. **Turn the channel off
first:** a rolled-back node still sees the newer build, and with
`update.auto: true` (the default) it reinstalls it within one
`check_interval` — indistinguishable, to the operator, from the rollback
not having worked.

```bash
# 1. `update.auto: false` in /etc/propmonitor/config.yaml, or the
#    auto-update toggle in Settings → software updates.
# 2. Put the previous binary back:
sudo -u propmonitor mv /opt/propmonitor/bin/propmonitor.prev \
                       /opt/propmonitor/bin/propmonitor
sudo systemctl restart propmonitor
```

`propmonitor.prev` is only ever written by a real install, and never with
the build being installed (step 1 above), so it stays the last build that
ran on this node.

---

## Versioning

Three independent version numbers appear in this document:

- **The propmonitor LAN API (§1, §2) is unversioned.** It ships with the
  binary and its only client is the embedded web UI, so it changes with the
  release rather than behind a URL prefix.
- **The microwaveprop REST API (§4, §5) is `v1`**, in the path
  (`/api/v1/…`). Breaking changes arrive as `/api/v2/…`; additive ones (new
  optional request fields, new response fields) land in place.
- **The sync protocol envelope (§5) is `v: 1`.** Microwaveprop enforces it —
  a node→prop frame carrying any other `v` earns an `unsupported_version`
  error frame. The node is the lenient half: it stamps `v: 1` on everything
  it sends and ignores `v` on everything it receives, dispatching purely on
  `type`, so a bump reaches it as new frame types rather than a hard break.

In every direction, ignore unknown JSON fields rather than treating them as
errors. Unknown frame types are logged and skipped on both sides.
