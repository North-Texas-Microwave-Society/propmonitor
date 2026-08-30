# propmonitor

Headless beacon-signal-level monitor. Tunes an SDR, measures noise floor +
signal level + SNR over a configurable integration window, and uploads the
measurements to [microwaveprop](https://prop.w5isp.com) so beacon strength
can be correlated with weather over time.

Includes a live web UI — waterfall, dBFS readout, settings form — on port
`5760`, reachable at `http://<host-ip>:5760`.

The deployment target is a headless Linux box running 24/7. macOS is
supported for development only.

---

## Supported systems

The installer downloads a prebuilt binary. Those binaries are built against
**glibc 2.38**, which is what sets the floor below:

| Operating system                        | Architectures     | glibc | Prebuilt binary  |
| --------------------------------------- | ----------------- | ----- | ---------------- |
| Debian 13 (trixie)                      | x86_64, aarch64   | 2.41  | Yes — verified   |
| Ubuntu 24.04 LTS (noble)                | x86_64, aarch64   | 2.39  | Yes — verified   |
| Raspberry Pi OS 64-bit (trixie)         | aarch64           | 2.41  | Yes              |
| Debian 12 (bookworm)                    | —                 | 2.36  | No — build from source |
| Raspberry Pi OS (bookworm, 32- or 64-bit) | —               | 2.36  | No — build from source |
| Ubuntu 22.04 LTS (jammy)                | —                 | 2.35  | No — build from source |
| Other Linux distributions               | —                 | —     | No — build from source |
| macOS                                   | —                 | —     | Development only; no service, no installer |

"Verified" means a clean container image of that release was taken from
nothing to a running, HTTP-answering service using the command below.

On a too-old system the installer stops with an explicit message rather than
installing something that cannot start. Nothing is left behind when it does.

Raspberry Pi 1 and Pi Zero W are **armv6** and have no prebuilt binary at
all. Pi 2 and newer are fine.

---

## Supported SDRs

propmonitor talks to hardware through [SoapySDR](https://github.com/pothosware/SoapySDR),
so anything with a SoapySDR driver module works. The `driver:` field in
`config.yaml` takes SoapySDR device arguments.

### Works out of the box

The installer sets up the RTL-SDR driver, so these need no extra steps:

| Hardware                                                    | `driver:` value |
| ----------------------------------------------------------- | --------------- |
| RTL-SDR Blog V3 and V4                                       | `rtlsdr`        |
| NooElec NESDR (Smart, SMArTee, Nano)                          | `rtlsdr`        |
| Generic RTL2832U DVB-T sticks (E4000, R820T, R820T2, FC0012/13) | `rtlsdr`      |

With more than one dongle attached, pin the one you want by serial:

```yaml
driver: "rtlsdr,serial=03340219"
```

Run `rtl_eeprom` to read or set a dongle's serial.

> The RTL-SDR Blog **V4** needs librtlsdr 2.0 or newer. Debian 13 ships
> 2.0.2 and Ubuntu 24.04 ships 2.0.1, so both are fine. On older
> distributions a V4 enumerates but produces no signal.

### Works after installing one more package

Driver names below were read off `SoapySDRUtil --info` on Debian 13, not
guessed:

| Hardware                        | `driver:` value | Package                        |
| ------------------------------- | --------------- | ------------------------------ |
| Airspy R2 / Airspy Mini         | `airspy`        | `soapysdr-module-airspy`       |
| HackRF One                      | `hackrf`        | `soapysdr-module-hackrf`       |
| Nuand bladeRF                   | `bladerf`       | `soapysdr-module-bladerf`      |
| LimeSDR / LimeSDR Mini          | `lime`          | `soapysdr-module-lms7`         |
| Ettus USRP (via UHD)            | `uhd`           | `soapysdr-module-uhd`          |
| Mirics MSi2500 devices          | `miri`          | `soapysdr-module-mirisdr`      |
| OsmoSDR hardware                | `osmosdr`       | `soapysdr-module-osmosdr`      |
| RFSpace SDR-IQ / SDR-IP / NetSDR | `rfspace`      | `soapysdr-module-rfspace`      |
| Red Pitaya                      | `redpitaya`     | `soapysdr-module-redpitaya`    |
| A SoapySDR server on another host | `remote`      | `soapysdr-module-remote`       |
| Sound card as an SDR            | `audio`         | `soapysdr-module-audio`        |

```bash
sudo apt install soapysdr-module-airspy   # or soapysdr-module-all for every driver
sudo systemctl restart propmonitor
```

### Needs manual work

**SDRplay** (RSP1A, RSPdx, RSPduo, …) is *not* packaged by Debian or Ubuntu —
the vendor API is proprietary. Using one means installing SDRplay's own API
package and building [SoapySDRPlay3](https://github.com/pothosware/SoapySDRPlay3)
from source. Once its module is present, set `driver: "sdrplay"`.

To see what your system actually detects:

```bash
SoapySDRUtil --find     # devices
SoapySDRUtil --info     # loaded driver modules
```

---

## Installing (operators)

This is the normal path. One command, on a clean machine:

```bash
curl -sSL https://raw.githubusercontent.com/North-Texas-Microwave-Society/propmonitor/main/install.sh | sudo bash
```

It installs every runtime dependency, downloads the latest release binary,
creates a locked-down system user and a systemd service, and asks a short
list of configuration questions. Nothing else needs to be installed first.

Specifically, it:

- installs SoapySDR, the RTL-SDR driver module, and the CA certificate
  bundle that beacon uploads need,
- blacklists the `dvb_usb_rtl28xxu` kernel driver — an RTL-SDR dongle
  presents itself as a DVB-T receiver, and if the kernel claims it first,
  the SDR fails with `usb_claim_interface error -6`. Neither Debian nor
  Ubuntu ships this blacklist,
- creates the `plugdev` group if the image lacks it and adds the service
  user to it, so the daemon can open the dongle without running as root,
- reloads udev and re-triggers USB, so a dongle that is already plugged in
  works immediately — no reboot,
- verifies the service is actually answering HTTP before reporting success.

Afterwards:

| What            | Where                                           |
| --------------- | ----------------------------------------------- |
| Web UI          | `http://<your-ip>:5760`                         |
| Logs            | `sudo journalctl -u propmonitor -f`             |
| Config          | `sudo nano /etc/propmonitor/config.yaml`        |
| Binary          | `/opt/propmonitor/bin/propmonitor`              |
| Restart         | `sudo systemctl restart propmonitor`            |
| Service status  | `systemctl status propmonitor`                  |

### Installing a managed monitor (one line, nothing to answer)

A monitor's page on [prop.w5isp.com](https://prop.w5isp.com) hands out the
command for *that* monitor, with its token already in it:

```bash
curl -sSL https://raw.githubusercontent.com/North-Texas-Microwave-Society/propmonitor/main/install.sh \
  | sudo PROPMONITOR_NONINTERACTIVE=1 PROPMONITOR_MONITOR_TOKEN=<token> bash
```

`PROPMONITOR_MONITOR_TOKEN` seeds `microwaveprop.monitor_token`, and a token
is all the identity a node needs: config sync starts as soon as one exists,
the website recognises the monitor it belongs to, and the frequency, driver,
beacon and grid square are pushed down from that monitor's page. So the
installer's questions are exactly the ones the website answers — hence
`PROPMONITOR_NONINTERACTIVE=1`, which takes the defaults for all of them.
Uploads themselves begin once the pushed config has arrived, since a
measurement needs the beacon and the grid square it was taken with.

An environment token outranks the one already in `config.yaml`, which is how
the same command re-points a node at a different monitor. Everything else
still comes from the file, so a re-run without the variable changes nothing.

### Staying up to date

**A node keeps itself current.** It follows the `main` branch: every push
rebuilds the binaries, and each node checks hourly, installs a new build and
restarts itself in place. Nothing to run, no reboot, no reinstall.

"In place" is literal — the daemon replaces its own process image, so the
PID never changes and systemd sees no restart. The interruption is the same
as any restart: the SDR is reopened and the live waterfall history resets,
which for a 24/7 beacon monitor costs one measurement period.

The web UI shows the running build and what the channel offers, with
**Check for update** and **Install** buttons for doing it on your schedule.
Settings → *software updates* has the three knobs:

```yaml
update:
  enabled: true          # watch for new builds at all
  auto: true             # install them without being asked
  check_interval: 3600   # seconds between checks (minimum 60)
```

With `auto: false` the node still tells you an update is waiting and leaves
the decision to you.

Something wrong with a new build? The previous binary is kept right next to
the current one — but **turn auto-update off first.** A rolled-back node
still sees the newer build on the channel, and with `auto: true` (the
default) it reinstalls it on the next check, which looks exactly like the
rollback having failed:

```bash
# 1. Stop the channel: Settings → software updates → uncheck auto-update,
#    or set `update.auto: false` in /etc/propmonitor/config.yaml.
# 2. Put the previous binary back:
sudo -u propmonitor mv /opt/propmonitor/bin/propmonitor.prev \
                       /opt/propmonitor/bin/propmonitor
sudo systemctl restart propmonitor
```

Downloads are verified against a SHA-256 published with the release, and
the new binary has to load this node's own config in a preflight run before
it is installed — so a corrupted download, a build that cannot start on this
system, and a build that would reject the config are all refused rather than
installed.

To check a config without starting anything:

```bash
/opt/propmonitor/bin/propmonitor --check-config /etc/propmonitor/config.yaml
```

### Re-running the installer

Safe. Existing settings become the defaults for each question, so holding
Enter through the prompts changes nothing. It also upgrades the binary to
the latest release — though with self-update on, a node is already there.

### Unattended installs

Accept every default without prompting:

```bash
curl -sSL https://raw.githubusercontent.com/North-Texas-Microwave-Society/propmonitor/main/install.sh \
  | sudo PROPMONITOR_NONINTERACTIVE=1 bash
```

Add `PROPMONITOR_MONITOR_TOKEN=<token>` to bind the node to a monitor in the
same command — see *Installing a managed monitor* above.

### Uninstalling

```bash
sudo systemctl disable --now propmonitor
sudo rm -f /etc/systemd/system/propmonitor.service \
           /etc/modprobe.d/propmonitor-rtlsdr.conf \
           /opt/propmonitor/bin/propmonitor /usr/local/bin/sdr_diag
sudo systemctl daemon-reload
sudo userdel propmonitor
sudo rm -rf /etc/propmonitor        # deletes your config and upload token
sudo rm -rf /opt/propmonitor
```

---

## Configuration

Everything in `/etc/propmonitor/config.yaml` is also editable from the web
UI. Saving there rewrites this file and applies the change immediately by
restarting the SDR worker.

```yaml
frequency: 28330000            # Hz — tuned center frequency
mode: beacon                   # usb | lsb | am | nfm | wfm | cw | beacon
driver: "rtlsdr"               # SoapySDR args, e.g. "rtlsdr,serial=03340219"
sample_rate: 250000            # Hz — minimum 250000
# gain: 10                     # dB. Comment this line out to use AGC.
ppm: 0                         # 0 for TCXO devices, 1-2 for plain crystal
period_seconds: 60             # measurement integration window, minimum 5

beacon:                        # used when mode == beacon
  offset_hz: 0                 # passband center, relative to `frequency`
  bandwidth_hz: 50             # narrow window for a tight CW carrier

http:
  bind: "0.0.0.0:5760"         # LAN-accessible on port 5760

update:                        # self-update, see "Staying up to date"
  enabled: true
  auto: true
  check_interval: 3600         # seconds, minimum 60

# Uploads to prop.w5isp.com. All three fields are required.
# microwaveprop:
#   enabled: true
#   gridsquare: "FN31pr"       # Maidenhead grid square, 4-20 characters
#   monitor_token: ""          # from https://prop.w5isp.com setup page
#   beacon_id: ""              # UUID of the beacon being monitored
```

Things worth knowing:

- **AGC is the absence of `gain`, not `gain: auto`.** The value is parsed as
  a number, so a literal `auto` makes the daemon refuse to start. Comment
  the line out instead.
- `sample_rate` has a hard floor of 250000; below that the waterfall bin
  width gets too coarse.
- `period_seconds` has a hard floor of 5.
- The whole `microwaveprop` block is optional. Leave it commented out to run
  the UI without reporting anywhere. Uploads need all three of `gridsquare`,
  `monitor_token` and `beacon_id`; the setup form in the web UI has a
  "Send beacon reports" checkbox to pause reporting without erasing them.
- The ingest URL is not configurable — it is `MICROWAVEPROP_ENDPOINT` in
  `src/uploader.rs`.
- **Managed monitors are configured from the website.** If the monitor was
  created on prop.w5isp.com as a *managed node*, the site holds the
  authoritative config and pushes changes to this daemon over a WebSocket
  (with HTTP polling as a backup). The LAN UI stays fully editable — local
  edits are pushed back up, last write wins. The daemon also reports its
  LAN address so the website can link to this UI. `config_version` under
  `microwaveprop:` is bookkeeping written by the daemon; don't hand-edit
  it. Nothing here activates until a `monitor_token` is set, so a
  self-service install is unaffected. Protocol: `api.md` §5.
- `http.bind` is deliberately **not** synced: a bad bind pushed from the
  website would make this UI unreachable with no way back in.
- The `update` block is node-local: a managed monitor's website config never
  touches it, so a node's update policy stays a local decision.

See [`api.md`](./api.md) for the full REST/WebSocket/upload contract.

---

## Troubleshooting

**No devices found.**

```bash
SoapySDRUtil --find          # does SoapySDR see it at all?
rtl_test -t                  # does the dongle itself respond?
lsusb | grep -i realtek      # is it even enumerating on USB?
```

**`usb_claim_interface error -6`** — the kernel DVB driver still owns the
dongle. The installer writes a blacklist for this; if the module was already
loaded and in use it cannot always be evicted live, so reboot:

```bash
lsmod | grep dvb_usb_rtl28xxu
sudo reboot
```

**Service will not stay up.** The daemon exits non-zero on a bad config, and
systemd retries every 10 seconds forever. The reason is in the journal:

```bash
sudo journalctl -u propmonitor -n 40 --no-pager
```

**Uploads never succeed.** Check `gridsquare`, `monitor_token` and
`beacon_id` are all set, and that `ca-certificates` is installed — TLS fails
without a trust store. Failed uploads are retried from an in-memory queue
holding up to 24 h of measurements, with backoff from 1 s to 5 min. The
queue does not survive a restart.

**Waterfall looks wrong.** Rule out the web/DSP path with the standalone
diagnostic tool, which talks to the device directly:

```bash
sdr_diag --driver rtlsdr --freq 28330000 --rate 1000000 --gain 10 --duration 30
```

It prints per-second RMS, top spectral peaks, and a small ASCII spectrum
plot around DC. Other flags: `--iqcorr`, `--dc-removal`, `--bandwidth`,
`--agc`, `--extra-args`. Unrecognised flags abort.

---

## Local development

Lower priority than the above — this section is for working on propmonitor
itself, not for running it.

### Prerequisites

**Rust 1.85 or newer.** That floor comes from `reqwest` 0.13. Install via
[rustup](https://rustup.rs) rather than apt: Ubuntu 24.04 packages rustc
1.75, which is too old. Debian 13 packages 1.85, which only just qualifies.

SoapySDR development headers, `pkg-config` (the `soapysdr-sys` build script
locates the library with it), a C toolchain for linking, and the driver
module for whatever dongle you have:

```bash
# Debian / Ubuntu
sudo apt install build-essential pkg-config libsoapysdr-dev soapysdr-module-rtlsdr

# macOS
brew install pkg-config soapysdr soapyrtlsdr
```

### Build and run

```bash
cargo build --release
./target/release/propmonitor                  # uses ./config.yaml
./target/release/propmonitor path/to.yaml
```

Then open `http://127.0.0.1:5760`. The bundled `config.yaml` binds
`0.0.0.0:5760`.

### Checks

```bash
cargo test
cargo test tone_in_passband_has_high_snr      # a single test
cargo clippy
cargo fmt
```

The web UI is plain embedded HTML/CSS/JS under `src/web/` with no build
step, so there is no npm anything.

### Releasing

Development happens on a private Forgejo instance which push-mirrors to the
public GitHub repository; GitHub Actions builds the binaries there. There
are two channels, and both matter:

**`main` → the rolling channel.** Every push to `main` runs
`.github/workflows/latest.yml`: it builds x86_64, aarch64 and armv7 Linux
binaries and republishes the `latest` GitHub Release with the raw binaries,
the tarballs, and `propmonitor-manifest.json`. **Deployed nodes install
this automatically** (see *Staying up to date* and `api.md` §6), so a push
to `main` reaches the fleet within the hour. `install.sh` resolves "latest
release" to the same build, which keeps a fresh install and a self-updated
node on identical binaries.

A push to `main` is therefore a deployment. `cargo test` before pushing.

**Tags → archival releases.** Pushing a version tag runs
`.github/workflows/release.yml`, which attaches the tarballs to a
`vX.Y.Z` release:

```bash
cargo test && cargo build --release
git tag v0.0.N && git push origin v0.0.N
```

Both workflows set `PROP_BUILD_COMMIT`, which `build.rs` bakes into the
binary as its update identity and as the flag marking it an official build.
A local `cargo build` is not marked, so a development binary is never
auto-replaced by a release one.

Because those runners are Ubuntu 24.04, the published binaries require glibc
2.38 — that is what the support table at the top of this file reflects.
Lowering the floor means changing the build images in `docker/`.

To exercise the update path without touching the real channel, point a node
at a local one:

```bash
PROPMONITOR_MANIFEST_URL=http://127.0.0.1:8099/propmonitor-manifest.json \
  ./target/release/propmonitor config.yaml
```

Architecture notes for contributors are in [`CLAUDE.md`](./CLAUDE.md).

---

## License

GPL-3.0-or-later. See [`LICENSE`](./LICENSE).
