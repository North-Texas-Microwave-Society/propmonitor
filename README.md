# propmonitor

Headless beacon-signal-level monitor. Tunes an SDR (RTL-SDR, SDRplay,
anything SoapySDR can drive), measures noise floor + signal level + SNR
over a configurable integration window, and uploads the measurements to
[microwaveprop](https://prop.w5isp.com) so beacon strength can be
correlated with weather over time.

Includes a live web UI (waterfall, dBFS readout, settings form) on port
`5760`, reachable at `http://<host-ip>:5760`.

## Quick install (Debian 13 / Raspberry Pi OS)

```bash
curl -sSL https://raw.githubusercontent.com/graham/propmonitor/main/install.sh | sudo bash
```

This single command installs everything: system dependencies, the latest
binary release, a systemd service, and walks you through initial
configuration. After it finishes:

- **Web UI:** `http://<your-pi-ip>:5760`
- **Logs:** `sudo journalctl -u propmonitor -f`
- **Edit config:** `sudo nano /etc/propmonitor/config.yaml`
- **Restart:** `sudo systemctl restart propmonitor`

The installer is safe to re-run — it won't overwrite your `config.yaml`.

## Build from source

The deployment target is a headless Linux box. Building locally on macOS
works for development:

### Requirements

- Rust stable
- [SoapySDR](https://github.com/pothosware/SoapySDR) + the driver module
  for your dongle (`soapysdr-module-rtlsdr`, `soapysdr-module-sdrplay`,
  etc.)

### macOS

```bash
brew install soapysdr soapyrtlsdr
```

### Debian/Ubuntu Linux

```bash
sudo apt install libsoapysdr-dev soapysdr-module-rtlsdr librtlsdr-dev
```

## Build & run

```bash
cargo build --release
./target/release/propmonitor               # uses ./config.yaml
./target/release/propmonitor my-config.yaml
```

Open your browser to
`http://127.0.0.1:5760`. The web UI exposes everything: live waterfall,
signal/noise readout, full settings form (changes are persisted to
`config.yaml` and apply immediately via worker restart).

## Configuration

`config.yaml`:

```yaml
frequency: 28330000           # Hz, tuned center frequency
mode: beacon                   # usb | lsb | am | nfm | wfm | cw | beacon
driver: "rtlsdr,serial=…"      # SoapySDR driver args
sample_rate: 250000            # Hz
gain: 10                       # dB; omit for AGC
ppm: 0
period_seconds: 60             # measurement integration window

beacon:                        # required when mode == beacon
  offset_hz: 0                 # passband center, relative to `frequency`
  bandwidth_hz: 50             # narrow window for tight CW beacon carriers

http:
  bind: "0.0.0.0:5760"         # LAN-accessible by default

# Optional: enable uploads to microwaveprop. Omit to run UI-only.
# Toggle on/off with the "Send beacon reports" checkbox in the web UI.
# The ingest URL is hardcoded in src/uploader.rs (MICROWAVEPROP_ENDPOINT).
# microwaveprop:
#   enabled: true
#   gridsquare: "FN31pr"         # REQUIRED — Maidenhead grid square of receiver
#   monitor_token: "…"           # from https://prop.w5isp.com setup page
#   beacon_id: "…"               # UUID of the beacon being monitored
```

See [`api.md`](./api.md) for the full REST/WebSocket/upload contract.

## Diagnostics

`sdr_diag` is a standalone tool for debugging device-level issues
without going through the main server:

```bash
./target/release/sdr_diag --driver "rtlsdr" --freq 28330000 --rate 1000000 --gain 10 --duration 30
```

It prints per-second RMS, top spectral peaks, and a small ASCII spectrum
plot around DC. Useful when the waterfall in the web UI looks wrong and
you want to rule out the web/DSP path.

## License

GPL-3.0-or-later. See [`LICENSE`](./LICENSE).
