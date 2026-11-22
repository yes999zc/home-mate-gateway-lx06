# Contributing to Home Mate · 小瑞

Thank you for your interest in contributing to **Home Mate · 小瑞** (https://mymate.homes).

## Repository Matrix

- **[yes999zc/home-mate](https://github.com/yes999zc/home-mate)** — Core voice brain (`hm-core`), signed release manager (`hm-release`), Vue 3 web console (`web/`), Echo Dot 2 provisioning script (`tools/hm-provision.sh`), and Northbound v2 protocol contract.
- **[yes999zc/home-mate-gateway-lx06](https://github.com/yes999zc/home-mate-gateway-lx06)** — Device Gateway for Xiaomi AI Speaker Pro (`LX06`, Amlogic A113X `aarch64`) with 100% on-speaker execution, zero stock firmware modification, and wake-word system switching.
- **[yes999zc/home-mate-gateway-dot2](https://github.com/yes999zc/home-mate-gateway-dot2)** — Device Gateway for Amazon Echo Dot 2 (`RS03QR`, MT8163 7-mic array).

## Architectural Boundary Rules

1. **Core vs. Gateway Separation**:
   - `hm-core` never contains hardware-model branches (no `if model == "LX06"` in Core).
   - Gateways own ALSA capture/playback, AEC, LED rings, physical buttons, and hardware voicing EQ, communicating with `hm-core` strictly over Northbound Protocol v2 (`127.0.0.1:8932`).
2. **Zero Credentials or Personal Data in Git**:
   - Never commit real LAN topology IPs, API keys, `.token` files, `.db` files, or `.wav` voice recordings. Use RFC 5737 / `192.168.1.x` placeholders in tests and examples.
3. **Quality Gates**:
   - Run `cargo fmt --all -- --check`, `python3 tools/line_gate.py`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace` before opening a Pull Request.

## Contributor License Agreement (CLA)

By submitting a Pull Request or patch to any Home Mate repository, you agree that:
1. Your contribution is your original work and you have the legal right to grant this license.
2. You grant the Project Maintainer (Bakey Zhang / `@yes999zc`) a perpetual, worldwide, non-exclusive, royalty-free, irrevocable license to use, modify, sublicense, and re-distribute your contribution as part of Home Mate under both the open-source license (`AGPL-3.0` / `Apache-2.0`) and commercial licensing terms.

## Contact

- **Email**: `zhangcong1985@gmail.com` / `yes999zc@163.com`
- **Website**: [https://mymate.homes](https://mymate.homes)
