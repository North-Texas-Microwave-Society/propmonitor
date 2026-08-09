# What propmonitor sends to microwaveprop

This is the wire contract the propmonitor client emits. Use it to build
the matching `POST /api/v1/beacon-monitor/measurements` endpoint on the
microwaveprop side. Everything here is what the client *actually* sends
today (commit `f236674`); the source of truth on the client is
`src/uploader.rs::WireMeasurement` and `src/uploader.rs::build_wire_measurement`.

---

## 1. Endpoint

```
POST https://prop.w5isp.com/api/v1/beacon-monitor/measurements
Authorization: Bearer <monitor_token>
Content-Type: application/json
```

- **Method:** `POST`
- **Path:** `/api/v1/beacon-monitor/measurements`
- **Host:** hardcoded as `prop.w5isp.com` on the client
  (`src/uploader.rs::MICROWAVEPROP_ENDPOINT`). Every install reports to
  the same server — only the per-station token + UUID differ.
- **Auth:** Bearer token in the `Authorization` header. The token is
  the 32-byte base64-url string returned **once** by
  `POST /api/v1/me/beacon-monitors` on the microwaveprop side. The same
  bearer-token plug already used for the rest of v1 API
  (`lib/microwaveprop_web/api/auth.ex`) handles this.
- **Body:** JSON object, one measurement per POST. Never batched.

The client uses a 15-second request timeout and `rustls-tls` (no system
OpenSSL dependency).

---

## 2. Request body

Exact shape, with realistic values:

```json
{
  "beacon_id":              "00000000-0000-0000-0000-000000000001",
  "frequency_hz":           28330000,
  "measured_at":            "2026-05-13T15:30:00Z",
  "integration_s":          60,
  "passband_hz":            300.0,
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

### Field-by-field

| Field | JSON type | Required | Range / format | Meaning |
|---|---|---|---|---|
| `beacon_id` | string | yes | UUID (typically v4/v7 string) | The beacon this measurement is for. Canonical key. The client never sends an empty value — uploads are paused if the operator hasn't filled this in. |
| `frequency_hz` | integer | yes | > 0 | The SDR's **tuned** center frequency. Not necessarily the beacon's nominal frequency — the operator may intentionally tune slightly off (e.g. CW offset, calibration). |
| `measured_at` | string | yes | ISO-8601 UTC, `YYYY-MM-DDTHH:MM:SSZ` | Timestamp at the **start** of the integration window (when the worker called `start_window` for this measurement). |
| `integration_s` | integer | yes | ≥ 5 (validated client-side) | Window length in seconds. Equal to `period_seconds` in the operator's config. Default 60. |
| `passband_hz` | number | yes | > 0, ≤ `sample_rate / 2` | The bandwidth of the in-band power window. Operator picks this wide enough to cover whichever modes the beacon uses (CW ≈ 50 Hz, Q65 ≈ 300 Hz, both ≈ 300 Hz). |
| `gain_db` | number | yes | typically 0–50 | The SDR's **actually reported** gain at the moment the upload is built. If AGC is on, this is whatever the tuner settled on. Used by the server to detect operator gain changes and rebaseline dBFS trends. |
| `noise_floor_dbfs` | number | yes | typically −140 to −60 | Median of out-of-passband bin power, in dBFS. Computed over averaged FFT frames with a guard region equal to `passband_hz / 2` on each side of the signal window. |
| `signal_peak_dbfs` | number | yes | typically −120 to −20 | Peak in-passband power across **detected-on-air** frames, in dBFS. See gating note below. |
| `signal_avg_dbfs` | number | yes | typically −120 to −20 | Mean in-passband power across **detected-on-air** frames, in dBFS. Mode-invariant — see gating note. |
| `snr_peak_db` | number | yes | typically 0 to 60 | `signal_peak_dbfs − noise_floor_dbfs`. |
| `snr_avg_db` | number | yes | typically 0 to 60 | `signal_avg_dbfs − noise_floor_dbfs`. |
| `signal_active_fraction` | number | yes | 0.0 – 1.0 | Fraction of FFT frames in the window where in-band power exceeded `noise_floor + 3 dB`. `1.0` = continuous carrier; `~0.5` = 50%-keyed CW; `0.0` = nothing heard. |
| `propmonitor_version` | string | yes | semver from `Cargo.toml` | Build version of the client. Currently `0.1.0`. Diagnostic only — helps explain stat-distribution shifts that correlate with client upgrades. |

All numeric fields are `f64` on the client and serialize as JSON
numbers; integers serialize without a fractional part (`60` not `60.0`).

### Signal-present gating — important

`signal_peak_dbfs`, `signal_avg_dbfs`, `snr_peak_db`, and `snr_avg_db`
are computed **only** over frames whose in-band power exceeded
`noise_floor + 3 dB` (a ×2 linear multiplier against the median per-bin
noise estimate). This makes the signal stats mode-invariant: a
30%-keyed CW beacon and a continuous Q65 transmission with the same TX
power produce the same `signal_avg_dbfs`. The duty cycle shows up in
`signal_active_fraction` instead.

**Fallback:** if no frames cross the threshold,
`signal_active_fraction == 0.0` and the peak/avg fall back to ungated
values (which sit near the noise floor; `snr_*_db` ≈ 0 dB). The server
should treat that case as "no signal heard" — likely filter out of
trend lines or store with an explicit absent-signal flag.

### Calibration

`dBFS` is uncalibrated — it's relative to the SDR ADC full scale and
depends on `gain_db`. To convert to absolute power (dBm) the server may
keep a per-monitor calibration offset (possibly indexed by `gain_db`);
that calibration is not configured on the client and not included in
the upload. The client does not send dBm.

---

## 3. Responses

The client interprets responses as follows (`src/uploader.rs::post_one`
and `src/uploader.rs::classify_status`):

| Status | Server meaning (proposed) | Client behavior |
|---|---|---|
| `204 No Content` | Accepted, recorded. Server should update the monitor's `last_seen_at`. | Mark measurement uploaded; reset backoff to 1 s. |
| `200 OK` (with or without body) | Same as 204. | Same as 204 — any 2xx is success. |
| `400 Bad Request` | Malformed body. | Log; drop measurement (won't retry — schema mismatch won't fix itself). |
| `401 Unauthorized` | Bad / missing / revoked monitor token. | Log; drop measurement. Operator needs to update the token. |
| `403 Forbidden` | `monitor_token` is valid but doesn't authorize reporting for the supplied `beacon_id`, or the beacon is not approved/on-the-air. | Log; drop measurement. |
| `404 Not Found` | Unknown `beacon_id`. | Log; drop measurement. |
| `429 Too Many Requests` | Rate limited. | Keep queued and retry with exponential backoff (1 s → 5 min cap). |
| `5xx` | Server-side failure. | Enqueue; retry with exponential backoff (1 s → 5 min cap). |
| network error / timeout | Unreachable. | Same as `5xx`. |

A successful response body is **ignored** by the client. `204 No
Content` is the simplest correct answer.

### Rate

One measurement per POST, one POST per `integration_s` window per
monitor. With the default `integration_s = 60`, that's **one POST per
minute per active monitor**. The microwaveprop rate limiter should be
set to bucket the monitor token at this rate with comfortable headroom
(say 10 req/min per token) so retry storms after a 5xx don't trip it.

### Retry queue

The client holds a bounded in-memory retry queue (cap: 1440 entries =
24 h of one-per-minute measurements). When the queue is full, oldest
entries are dropped. The queue does **not** persist across propmonitor
restarts — that's by design; long outages should be handled server-side
or via a future durable-queue feature.

### `last_seen_at`

The server should update `BeaconMonitor.last_seen_at` to the current
server time on each accepted (2xx) POST. The client does not send a
"heartbeat" — measurements *are* the heartbeat. A monitor that hasn't
posted in N minutes is either down or has uploads disabled.

---

## 4. Things to validate server-side

1. **Bearer token resolves to a `BeaconMonitor`.** → otherwise 401.
2. **`beacon_id` exists and the monitor is authorized to report for it.**
   Open question for you: do you want a monitor to be bound to exactly
   one beacon at registration time, or to be able to report for any
   beacon by UUID? The client just sends the UUID and Bearer; either
   policy fits.
3. **`measured_at` parses as RFC 3339 / ISO-8601 UTC.** Reject if not
   `Z`-terminated; the client never emits offsets.
4. **`integration_s` in a sane range** (5–3600). The client validates
   ≥5 but doesn't enforce an upper bound.
5. **`signal_active_fraction` in `[0.0, 1.0]`.**
6. **`gain_db` in a sane range** (−10 to 80 covers every SoapySDR
   driver I'm aware of). Useful as a sanity check, not a hard reject.
7. **Body size cap.** The body is < 1 KB in practice; a 4 KB cap is
   plenty.

---

## 5. Example Elixir schema (sketch — adjust to your conventions)

This isn't required output, just a starting point matching the field
names exactly:

```elixir
defmodule Microwaveprop.Beacons.Measurement do
  use Ecto.Schema
  import Ecto.Changeset

  @primary_key {:id, :binary_id, autogenerate: true}
  @foreign_key_type :binary_id

  schema "beacon_measurements" do
    belongs_to :beacon, Microwaveprop.Beacons.Beacon
    belongs_to :monitor, Microwaveprop.BeaconMonitors.BeaconMonitor

    field :measured_at, :utc_datetime
    field :frequency_hz, :integer
    field :integration_s, :integer
    field :passband_hz, :float
    field :gain_db, :float

    field :noise_floor_dbfs, :float
    field :signal_peak_dbfs, :float
    field :signal_avg_dbfs, :float
    field :snr_peak_db, :float
    field :snr_avg_db, :float

    field :signal_active_fraction, :float
    field :propmonitor_version, :string

    timestamps(type: :utc_datetime)
  end

  @required ~w(beacon_id monitor_id measured_at frequency_hz integration_s
               passband_hz gain_db noise_floor_dbfs signal_peak_dbfs
               signal_avg_dbfs snr_peak_db snr_avg_db
               signal_active_fraction propmonitor_version)a

  def changeset(meas, attrs) do
    meas
    |> cast(attrs, @required)
    |> validate_required(@required)
    |> validate_number(:integration_s, greater_than_or_equal_to: 5, less_than_or_equal_to: 3600)
    |> validate_number(:signal_active_fraction, greater_than_or_equal_to: 0.0, less_than_or_equal_to: 1.0)
    |> validate_number(:passband_hz, greater_than: 0.0)
    |> foreign_key_constraint(:beacon_id)
    |> foreign_key_constraint(:monitor_id)
  end
end
```

Note that the client sends `beacon_id` as a string in the JSON body but
the Bearer token resolves the `monitor_id` — only the body's
`beacon_id` is on the wire.

---

## 6. Reference client source

If you want to read the exact code that produces every byte of this
payload:

- Wire struct: `src/uploader.rs::WireMeasurement`
- Build function: `src/uploader.rs::build_wire_measurement` (pure,
  unit-tested)
- Auth + POST: `src/uploader.rs::post_one`
- Retry loop: `src/uploader.rs::run`
- Status classification: `src/uploader.rs::classify_status` (2xx →
  accept, non-429 4xx → drop, 429/5xx/network → retry)
- DSP that produces the signal stats: `src/measure.rs::SpectrumAnalyzer::finalize`
  (the gating + active-fraction calculation in particular)

Send me anything you want tweaked on the wire side — the client side is
small enough (≈100 lines for the whole uploader) that schema shifts are
cheap before you ship the server endpoint.
