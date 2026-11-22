# Vendored SpeexDSP — echo canceller subset

Provenance: EchoMuse upstream tree `wilbowes/EchoMuse@6a7f6625ed3023a0d5a7a1990cef77a4a13081f1`
(emos-v0.10), directory `device/internal/aec/` — which itself bundles the
floating-point SpeexDSP sources. Files copied verbatim (no modifications):

- `src/mdf.c` + `include/speex/speex_echo.h` — the acoustic echo canceller
  (MDF) and its public API
- `src/fftwrap.c|h`, `src/kiss_fft.c|h`, `src/kiss_fftr.c|h`,
  `src/_kiss_fft_guts.h` — the FFT layer mdf.c builds on (`USE_KISS_FFT`)
- `src/arch.h`, `src/fixed_generic.h`, `src/math_approx.h`,
  `src/os_support.h`, `src/pseudofloat.h` — support headers
- `COPYING` — the Xiph.org/BSD-style license (copyright Xiph.org Foundation,
  Jean-Marc Valin, Analog Devices, CSIRO, David Rowe, EpicGames,
  Jutta Degener & Carsten Bormann). **Redistributions must preserve this
  notice**; it is registered in the dependency table (docs + HANDOFF) and
  the license file ships inside the source tree it covers.

Build (see ../build.rs, mirroring the cgo flags EchoMuse compiles these
with): `-DFLOATING_POINT -DUSE_KISS_FFT -DEXPORT=` — no config.h needed.
