# propmonitor

A small Rust CLI for monitoring radio propagation conditions with a software-defined radio. It tunes an SDR to a configured frequency and mode, captures IQ samples, and once per minute reports the noise floor, in-band signal level (both peak and average), and SNR in dB.

It's intended for unattended, long-running captures — for example, watching a beacon, a broadcast carrier, or a band segment over hours or days to get a sense of how propagation is shifting.

## Status

This is only validated to work on a Mac with an RSP1A SDR.

## Requirements

- Rust toolchain (stable)
- [SoapySDR](https://github.com/pothosware/SoapySDR) and the [SoapySDRPlay](https://github.com/pothosware/SoapySDRPlay3) driver
- An SDRplay RSP1A

## Usage

```bash
cargo run --release                    # uses ./config.yaml
cargo run --release -- my-config.yaml  # explicit config path
```

## Configuration

`config.yaml` controls the tuned frequency, demodulation mode, sample rate, and gain:

```yaml
frequency: 101100000   # Hz
mode: wfm              # usb | lsb | am | nfm | wfm | cw
sample_rate: 2000000   # optional, default 2_000_000
gain: 40               # optional, dB; omit for AGC
```

The `mode` selects the passband used for in-band signal measurement and for excluding those bins from the noise-floor estimate.
