# Building propmonitor on Windows

CI doesn't build Windows binaries; this guide walks through doing it
yourself on a Windows 10 or 11 PC. Total time on a clean machine is about
20–30 minutes (most of which is downloads + a one-time `cargo build`).

## What you'll end up with

- `propmonitor.exe` — the headless server + tray icon. Open the tray
  menu's **Open in browser** to reach the web UI.
- `SoapySDR.dll` and driver modules (e.g. `librtlsdrSupport.dll`) bundled
  next to the .exe so it can run on a machine without PothosSDR
  installed system-wide.

## Prerequisites

You need three things installed system-wide, in this order:

### 1. Rust + MSVC toolchain

The simplest path is the Chocolatey package `rust-ms`, which pulls in
both `rustup` and the Microsoft Visual C++ build tools that `bindgen`
needs.

Open an **administrator** PowerShell:

```powershell
# Install Chocolatey itself (skip if you already have it):
Set-ExecutionPolicy Bypass -Scope Process -Force
iex ((New-Object Net.WebClient).DownloadString('https://chocolatey.org/install.ps1'))

choco install rust-ms -y
```

Verify, in a **new** terminal (so PATH refreshes):

```powershell
rustc --version
cargo --version
```

Alternative (without Chocolatey): install
[**Visual Studio 2022 Build Tools**](https://visualstudio.microsoft.com/downloads/)
with the "Desktop development with C++" workload, then install
[**rustup**](https://rustup.rs/) and run `rustup default stable`.

### 2. PothosSDR (provides SoapySDR + drivers)

[PothosSDR](https://github.com/pothosware/PothosSDR/wiki) bundles
SoapySDR, the RTL-SDR module, SDRplay module, Airspy, HackRF, and most
common SDR drivers into a single Windows installer.

```powershell
choco install pothossdr -y
```

Or grab the latest installer manually from the
[PothosSDR releases page](https://downloads.myriadrf.org/builds/PothosSDR/)
and run it. **Default install path is `C:\Program Files\PothosSDR`** —
propmonitor's build script looks for it there automatically.

If you installed somewhere else, set this env var before building:

```powershell
$env:SOAPY_SDR_ROOT = "D:\path\to\PothosSDR"
```

### 3. USB driver for your dongle (Zadig)

RTL-SDR dongles ship as a TV-tuner device by default, which Windows
claims with its DVB driver. To use them for SDR you have to replace the
driver with WinUSB using [Zadig](https://zadig.akeo.ie/).

1. Plug the dongle in.
2. Run Zadig.
3. **Options → List All Devices**.
4. Pick **Bulk-In, Interface (Interface 0)** (or `RTL2838UHIDIR` —
   whichever matches your dongle).
5. Set the target driver to **WinUSB**.
6. Click **Replace Driver**.

You only do this once per dongle per PC. SDRplay devices have their own
installer and don't need Zadig.

## Build

In a regular (non-admin) PowerShell, from the repo root:

```powershell
git clone <your-repo-url> propmonitor
cd propmonitor
cargo build --release
```

The first build takes 3–5 minutes (downloads and compiles ~200 crates).
Subsequent rebuilds are seconds.

The compiled exe lands at `target\release\propmonitor.exe`.

## Bundle for distribution

The .exe links dynamically against `SoapySDR.dll`. To run it on a
machine without PothosSDR installed, copy a few DLLs next to the .exe:

```powershell
$dest = "dist"
mkdir $dest -Force | Out-Null

copy target\release\propmonitor.exe $dest\
copy target\release\sdr_diag.exe   $dest\
copy "C:\Program Files\PothosSDR\bin\SoapySDR.dll" $dest\

# Driver modules go in a subdirectory called `SoapySDR\modules<version>`.
# Find the actual modules path with:
#   "C:\Program Files\PothosSDR\bin\SoapySDRUtil.exe" --info
# It will print something like: Module found: …\SoapySDR\modules0.8\rtlsdrSupport.dll
mkdir "$dest\SoapySDR\modules0.8" -Force | Out-Null
copy "C:\Program Files\PothosSDR\bin\SoapySDR\modules0.8\rtlsdrSupport.dll" "$dest\SoapySDR\modules0.8\"

# librtlsdr DLL (the actual RTL-SDR driver the SoapySDR module wraps):
copy "C:\Program Files\PothosSDR\bin\rtlsdr.dll" $dest\

# Optional: copy other driver modules you want shipped, e.g. SDRplay:
# copy "C:\Program Files\PothosSDR\bin\SoapySDR\modules0.8\sdrPlaySupport.dll" "$dest\SoapySDR\modules0.8\"
# copy "C:\Program Files\PothosSDR\bin\sdrplay_api.dll" $dest\
```

`dist\` is now a fully self-contained directory you can zip and copy to
any Windows machine (which still needs Zadig run once per dongle).

## Run

```powershell
cd dist
.\propmonitor.exe config.yaml
```

You should see:

- A small antenna icon appear in the system tray (near the clock).
- Your default browser open to `http://127.0.0.1:5760`.
- Console output showing the SDR being opened and the device info line.

Right-click the tray icon for the menu (Open in browser / Quit).

## Common issues

| Symptom | Fix |
|---|---|
| `cargo build` error: "could not find SoapySDR" | PothosSDR not installed, or installed somewhere other than `C:\Program Files\PothosSDR`. Reinstall to the default path or set `$env:SOAPY_SDR_ROOT`. |
| `cargo build` error mentioning `libclang.dll` | bindgen needs libclang. The PothosSDR installer normally adds one, otherwise install LLVM via `choco install llvm`. |
| Build succeeds but running .exe says "STATUS_DLL_NOT_FOUND" | Either PothosSDR's `bin\` isn't on PATH, or you moved the .exe without bundling `SoapySDR.dll`. See "Bundle for distribution" above. |
| Running .exe says "no rtlsdr device found" | Either the dongle isn't plugged in, Zadig hasn't been run yet, or another program (CubicSDR, SDR#) has the device claimed — close it and retry. |
| Tray icon doesn't appear, browser does | Tray init silently fell back to headless mode (rare on Windows). Check the console for `tray unavailable` warnings. The web UI still works fine. |
| Antenna in the tray looks tiny / blurry on a 4K display | Known limitation of the hand-drawn 32×32 RGBA icon. The shape is still recognizable; if it bothers you, replace `make_antenna_icon` in `src/tray.rs` with a higher-resolution version (PNG decoded by the `image` crate). |

## Updating

Pull, rebuild, copy. There's no installer state to worry about; the only
persistent file is `config.yaml`, which the binary reads from its
working directory.

```powershell
git pull
cargo build --release
copy target\release\propmonitor.exe dist\
```
