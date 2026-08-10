# propmonitor API

This document is the integration contract for `propmonitor`. It covers four
surfaces:

1. The HTTP REST API served on the operator's LAN.
2. The WebSocket stream the browser UI consumes.
3. The on-disk `config.yaml` format.
4. The outbound HTTP POST contract to **microwaveprop** for uploading
   beacon-signal-level measurements.

Anyone integrating with propmonitor — extending the web UI, writing a
third-party dashboard, or implementing the matching ingest endpoint on the
microwaveprop side — should be able to do so from this file alone.

---

## 1. HTTP REST API

Bind: `0.0.0.0:5760` by default (configurable via `http.bind`). LAN-accessible,
no authentication. Intended for trusted networks.

All responses are `application/json` unless noted. Errors use the shape
`{"error": "<message>"}` with appropriate `4xx` / `5xx` status.

### `GET /`

Returns the embedded `index.html` for the single-page web UI.
Response: `200 text/html`.

### `GET /assets/{name}`

Returns one of the embedded static files (`app.js`, `style.css`).
Response: `200` with the appropriate `Content-Type`. `404` if `name` is not a
known asset.

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
    "beacon_id": "00000000-0000-0000-0000-000000000000"
  }
}
```

The `monitor_token` field is **redacted** on read — the response contains the
literal string `"redacted"` if a token is configured, or `null` if not. The
real token is only ever written via `PUT /api/config`.

### `PUT /api/config`

Replaces the active configuration. The request body must be a complete config
object in the same shape as `GET /api/config` returns. To leave the
`monitor_token` unchanged, send `"redacted"`; any other value (including
`null`) is treated as a real update.

Behavior:

1. Validate the body. On failure: `400 Bad Request` with an `error` message.
2. Atomically rewrite `config.yaml` (write to `config.yaml.tmp`, fsync,
   rename).
3. Signal the worker to stop, wait for it, spawn a new worker with the new
   config. The SoapySDR device is released and reopened.
4. Respond `200 OK` with the new config (token still redacted).

During step 3 the WebSocket stream pauses briefly. The new worker emits a
fresh `device_info` event when it comes up, which clients use to detect the
transition.

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

#### `device_info` — emitted once when the worker comes up

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

`bins.length == 1024`. Values are **linear power** (`|x|²`), not dB. The first
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
```

Validation rules:

- `frequency` > 0.
- `mode` must be one of the listed values.
- `sample_rate` >= 250000 (lower rates pin the waterfall FFT under 250 Hz/bin,
  unacceptable for visual band overview).
- `gain` either a number or omitted.
- `period_seconds` >= 5.
- `beacon.bandwidth_hz` > 0 and <= `sample_rate / 2`.
- `microwaveprop.enabled` must be `true` AND `monitor_token` must be non-empty AND `beacon_id` must be non-empty AND `gridsquare` must be non-empty for uploads to actually run. Any of these conditions false → uploads paused.

The in-tree YAML parser (`src/yaml.rs`) supports the subset used here:
scalars (numbers, quoted/unquoted strings, comments) and one level of nesting
indented two spaces. Lists are not used. Don't use anchors, aliases, or
multi-document syntax.

---

## 4. microwaveprop upload contract

This is the wire format the propmonitor uploader emits. The matching server
endpoint is **not yet implemented** in microwaveprop — this section is the
spec both sides should target.

### Endpoint

The full URL is hardcoded in propmonitor at `src/uploader.rs` —
`MICROWAVEPROP_ENDPOINT`. The default is:

```
POST https://prop.w5isp.com/api/v1/beacon-monitor/measurements
Authorization: Bearer <monitor_token>
Content-Type: application/json
```

`<monitor_token>` is the 32-byte base64-url string returned once by
`POST /api/v1/me/beacon-monitors` on the microwaveprop side
(`lib/microwaveprop/beacon_monitors.ex:30-37`). The same Bearer-token plug
already used for the rest of the v1 API
(`lib/microwaveprop_web/api/auth.ex`) handles this.

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
  "propmonitor_version":    "0.1.0"
}
```

Field semantics:

| Field | Type | Notes |
|---|---|---|
| `beacon_id` | string (UUID) | Identifies *which* beacon this measurement is for. Canonical key on the microwaveprop side. The same monitor_token can only legitimately report for the beacon its record is associated with; mismatches are server policy. |
| `gridsquare` | string | Maidenhead grid square of the RECEIVER station (e.g. `"FN31pr"`). 4–20 characters. Required for the server to correlate signal strength with propagation-path distance and bearing. |
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
| `propmonitor_version` | string | Build version of the reporting client. Diagnostic only — helps the server correlate stat shifts with client upgrades. |

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
side may keep a per-monitor calibration offset (possibly indexed by
`gain_db`); that calibration is not configured in propmonitor and not
included in the upload.

### Responses

| Status | Meaning | Client behavior |
|---|---|---|
| `204 No Content` | Accepted, recorded. Server should update the monitor's `last_seen_at`. | Mark measurement uploaded. |
| `400 Bad Request` | Malformed body or unknown `beacon_id`. | Log, drop measurement (don't retry — schema mismatch won't fix itself). |
| `401 Unauthorized` | Bad/missing/revoked monitor token. | Log, stop uploading until config is updated. |
| `429 Too Many Requests` | Rate limited. | Enqueue and retry with exponential backoff (1 s → 5 min cap). |
| `5xx` | Server-side failure. | Enqueue, retry with exponential backoff (1 s → 5 min cap). |
| network error / timeout | Unreachable. | Same as `5xx`. |

### Retry queue

The propmonitor uploader holds a bounded in-memory queue (default cap: 1440
entries = 24 h of one-per-minute measurements). When the queue is full,
oldest entries are dropped. The queue is not persisted across propmonitor
restarts.

### Rate / batching

One measurement per POST. If `period_seconds == 60`, the upload rate is
1/minute per monitor. Microwaveprop's per-token rate limit
(`lib/microwaveprop_web/api/rate_limiter.ex`) should be set with this in
mind; the default authenticated bucket is sufficient.

---

## Versioning

This document describes API **v0**. Breaking changes will be announced via a
new top-level major version in the URL (`/api/v2/…`). Additive changes
(new optional config fields, new optional JSON fields in responses) are
in-place. Clients should ignore unknown JSON fields.
