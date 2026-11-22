// hm-kws: streaming keyword-spotter sidecar for the dot2-d firmware (D/T4).
//
// Why a driver: the sherpa-onnx release CLIs only decode WAV files, and the
// firmware owns the audio device (post-AEC samples live in its memory), so
// live keyword spotting needs a process that accepts PCM on a pipe. This is
// that process: read raw s16le mono 16 kHz on stdin in the firmware's 80 ms
// block cadence, run the streaming zipformer keyword spotter, print one
// JSON line per event on stdout. The firmware supervises it and forwards
// detections north as facts; the *decision* to stop playback stays in the
// core (the gateway reports, the core decides).
//
// Latency accounting is built in, but it measures the DEVICE's freshness, not
// the model's algorithmic delay. `lag_ms` is how long the newest audio had
// been waiting when the hit was printed (queue + decode + scheduling) — the
// number an interruption budget actually bounds.
//
// 2026-11-15: the field used to be `detect_ms = fed_ms - last_token_ms`, on the
// assumption that sherpa reports absolute stream timestamps. It does not: the
// keyword result's timestamps are relative to the detected phrase, so the field
// printed `fed_ms - 2.8s` — hours on a long-running sidecar — and made people
// hunt a phantom "delayed report". Measured and removed, not re-derived.
//
// Exit code 0 = stdin EOF or SIGTERM (clean), 2 = model/config failure
// (the firmware must treat that as "KWS unavailable" and say so honestly).
//
// Build: ./build.sh (zig cc, aarch64-linux-gnu, links the official
// sherpa-onnx v1.12.15 shared release). Runs on emOS with the bundled glibc
// loader (the device has no /lib/ld-linux-aarch64.so.1; the spike proved
// the bundled-loader path works for glibc binaries).

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "c-api.h"

// Firmware uplink block: 80 ms at 16 kHz. Keeping the same cadence means
// one read() per block and no extra buffering on our side.
#define BLOCK_SAMPLES 1280
#define SAMPLE_RATE 16000

// Upper bound on how long a stop request can go unnoticed while no audio
// arrives. A muted device feeds no PCM at all (the firmware's capture read
// is parked in the codec), so a blocking read() is never woken by data and
// the SIGTERM handler alone cannot end this process — glibc's signal()
// installs SA_RESTART, and the EINTR branch below used to re-read instead of
// honouring the stop. 2026-11-14: that made "deploy a new keyword file"
// impossible on a muted device (reflex.deploy was accepted, the old spotter
// kept the old vocabulary, and only a firmware restart applied the file).
#define STOP_POLL_MS 250

static volatile sig_atomic_t g_stop = 0;

static void on_signal(int sig) {
  (void)sig;
  g_stop = 1;
}

static uint64_t monotonic_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (uint64_t)ts.tv_sec * 1000u + (uint64_t)ts.tv_nsec / 1000000u;
}

// Wait up to `timeout_ms` for stdin to become readable.
// 1 = readable (or the writer closed: read() will report EOF), 0 = timeout,
// -1 = poll error other than a signal. A signal is reported as 0 so the
// caller re-checks g_stop instead of treating it as a failure.
static int wait_stdin(int timeout_ms) {
  struct pollfd p;
  p.fd = STDIN_FILENO;
  p.events = POLLIN;
  p.revents = 0;
  int r = poll(&p, 1, timeout_ms);
  if (r < 0) return errno == EINTR ? 0 : -1;
  return r;
}

static void usage(const char *argv0) {
  fprintf(stderr,
          "usage: %s --tokens F --encoder F --decoder F --joiner F \\\n"
          "          --keywords-file F [--threshold 0.07] [--score 4.0]\n"
          "          [--trailing-blanks 0] [--num-threads 1] [--cpu N]\n"
          "          [--model-type zipformer2] [--name dot2-d]\n"
          "          [--vad-model F] [--vad-threshold 0.5] [--vad-gain 3.0]\n"
          "          [--vad-min-silence 0.001] [--vad-min-speech 0.001]\n"
          "          [--vad-window-size 512]\n"
          "\n"
          "stdin: raw s16le mono 16 kHz PCM, any block size.\n"
          "stdout: one JSON object per line (ready / keyword / vad / exit).\n"
          "stderr: human log lines prefixed [kws].\n",
          argv0);
}

static void print_json_str(const char *s) {
  putchar('"');
  if (s) {
    for (const unsigned char *p = (const unsigned char *)s; *p; p++) {
      switch (*p) {
        case '"':  fputs("\\\"", stdout); break;
        case '\\': fputs("\\\\", stdout); break;
        case '\n': fputs("\\n", stdout); break;
        case '\r': fputs("\\r", stdout); break;
        case '\t': fputs("\\t", stdout); break;
        default:
          if (*p < 0x20) printf("\\u%04x", *p);
          else putchar(*p);
          break;
      }
    }
  }
  putchar('"');
}

struct Args {
  const char *tokens, *encoder, *decoder, *joiner, *keywords_file;
  const char *model_type, *name;
  const char *vad_model;
  float threshold, score;
  float vad_threshold, vad_gain, vad_min_silence, vad_min_speech;
  int trailing_blanks, num_threads, cpu, vad_window_size;
  int pace;  // read at real time (test harnesses pipe faster than the model)
};

static int parse_args(int argc, char **argv, struct Args *a) {
  memset(a, 0, sizeof(*a));
  a->threshold = 0.07f;  // RUI production value (spike: 5/5 real speech)
  a->score = 4.0f;       // RUI production boost for the keyword line
  a->trailing_blanks = 0;  // no trailing blanks: the token IS the trigger
  a->num_threads = 1;
  a->cpu = -1;
  a->pace = 0;
  a->model_type = "zipformer2";  // avoids the CLI's double model load
  a->name = "dot2-d";
  a->vad_model = NULL;
  a->vad_threshold = 0.5f;
  a->vad_gain = 3.0f;  // matches hm-core silero::DEFAULT_GAIN for 3m soft-voice recall
  a->vad_min_silence = 0.001f;  // Zero selects sherpa-onnx's 500 ms silence default.
  a->vad_min_speech = 0.001f;   // Zero selects its 250 ms speech default; Core owns hysteresis.
  a->vad_window_size = 512;

  for (int i = 1; i < argc; i++) {
    const char *k = argv[i];
    const char *v = NULL;
    char key[64];
    const char *eq = strchr(k, '=');
    if (eq) {
      size_t n = (size_t)(eq - k);
      if (n >= sizeof(key)) return -1;
      memcpy(key, k, n);
      key[n] = '\0';
      k = key;
      v = eq + 1;
    } else if (!strcmp(k, "--pace")) {
      a->pace = 1;
      continue;
    } else if (!strcmp(k, "--help") || !strcmp(k, "-h")) {
      return 1;
    } else if (i + 1 < argc) {
      v = argv[++i];
    }
    if (!v) {
      fprintf(stderr, "[kws] option %s needs a value\n", k);
      return -1;
    }
    if (!strcmp(k, "--tokens")) a->tokens = v;
    else if (!strcmp(k, "--encoder")) a->encoder = v;
    else if (!strcmp(k, "--decoder")) a->decoder = v;
    else if (!strcmp(k, "--joiner")) a->joiner = v;
    else if (!strcmp(k, "--keywords-file")) a->keywords_file = v;
    else if (!strcmp(k, "--model-type")) a->model_type = v;
    else if (!strcmp(k, "--name")) a->name = v;
    else if (!strcmp(k, "--threshold")) a->threshold = strtof(v, NULL);
    else if (!strcmp(k, "--score")) a->score = strtof(v, NULL);
    else if (!strcmp(k, "--trailing-blanks")) a->trailing_blanks = (int)strtol(v, NULL, 10);
    else if (!strcmp(k, "--num-threads")) a->num_threads = (int)strtol(v, NULL, 10);
    else if (!strcmp(k, "--cpu")) a->cpu = (int)strtol(v, NULL, 10);
    else if (!strcmp(k, "--vad-model")) a->vad_model = v;
    else if (!strcmp(k, "--vad-threshold")) a->vad_threshold = strtof(v, NULL);
    else if (!strcmp(k, "--vad-gain")) a->vad_gain = strtof(v, NULL);
    else if (!strcmp(k, "--vad-min-silence")) a->vad_min_silence = strtof(v, NULL);
    else if (!strcmp(k, "--vad-min-speech")) a->vad_min_speech = strtof(v, NULL);
    else if (!strcmp(k, "--vad-window-size")) a->vad_window_size = (int)strtol(v, NULL, 10);
    else {
      fprintf(stderr, "[kws] unknown option %s\n", k);
      return -1;
    }
  }
  if (!(a->vad_gain > 0.0f && a->vad_gain <= 32.0f)) a->vad_gain = 3.0f;
  if (!a->tokens || !a->encoder || !a->decoder || !a->joiner || !a->keywords_file) {
    return -1;
  }
  return 0;
}

int main(int argc, char **argv) {
  struct Args args;
  int rc = parse_args(argc, argv, &args);
  if (rc != 0) {
    usage(argv[0]);
    return 2;
  }

  signal(SIGTERM, on_signal);
  signal(SIGINT, on_signal);
  signal(SIGPIPE, SIG_IGN);  // writer gone = EOF on read, not a crash

  if (args.cpu >= 0 && args.cpu < CPU_SETSIZE) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(args.cpu, &set);
    if (sched_setaffinity(0, sizeof(set), &set) != 0) {
      fprintf(stderr, "[kws] sched_setaffinity(cpu%d) failed: %s (continuing)\n",
              args.cpu, strerror(errno));
    } else {
      fprintf(stderr, "[kws] pinned to cpu%d\n", args.cpu);
    }
  }

  SherpaOnnxKeywordSpotterConfig config;
  memset(&config, 0, sizeof(config));
  config.feat_config.sample_rate = SAMPLE_RATE;
  config.feat_config.feature_dim = 80;
  config.model_config.transducer.encoder = args.encoder;
  config.model_config.transducer.decoder = args.decoder;
  config.model_config.transducer.joiner = args.joiner;
  config.model_config.tokens = args.tokens;
  config.model_config.num_threads = args.num_threads;
  config.model_config.provider = "cpu";
  config.model_config.debug = 0;
  config.model_config.model_type = args.model_type;
  config.max_active_paths = 4;
  config.num_trailing_blanks = args.trailing_blanks;
  config.keywords_score = args.score;
  config.keywords_threshold = args.threshold;
  config.keywords_file = args.keywords_file;

  uint64_t t0 = monotonic_ms();
  const SherpaOnnxKeywordSpotter *spotter = SherpaOnnxCreateKeywordSpotter(&config);
  uint64_t load_ms = monotonic_ms() - t0;
  if (!spotter) {
    fprintf(stderr, "[kws] model load failed (encoder=%s)\n", args.encoder);
    printf("{\"event\":\"error\",\"stage\":\"load\",\"load_ms\":%llu}\n",
           (unsigned long long)load_ms);
    fflush(stdout);
    return 2;
  }
  const SherpaOnnxOnlineStream *stream = SherpaOnnxCreateKeywordStream(spotter);
  if (!stream) {
    fprintf(stderr, "[kws] keyword stream create failed\n");
    printf("{\"event\":\"error\",\"stage\":\"stream\"}\n");
    fflush(stdout);
    SherpaOnnxDestroyKeywordSpotter(spotter);
    return 2;
  }

  const SherpaOnnxVoiceActivityDetector *vad = NULL;
  if (args.vad_model && args.vad_model[0] != '\0') {
    SherpaOnnxVadModelConfig vad_cfg;
    memset(&vad_cfg, 0, sizeof(vad_cfg));
    vad_cfg.silero_vad.model = args.vad_model;
    vad_cfg.silero_vad.threshold = args.vad_threshold;
    vad_cfg.silero_vad.min_silence_duration = args.vad_min_silence;
    vad_cfg.silero_vad.min_speech_duration = args.vad_min_speech;
    vad_cfg.silero_vad.window_size = args.vad_window_size;
    vad_cfg.silero_vad.max_speech_duration = 30.0f;
    vad_cfg.sample_rate = SAMPLE_RATE;
    vad_cfg.num_threads = 1;
    vad_cfg.provider = "cpu";
    vad_cfg.debug = 0;
    uint64_t vt0 = monotonic_ms();
    vad = SherpaOnnxCreateVoiceActivityDetector(&vad_cfg, 10.0f);
    uint64_t vload_ms = monotonic_ms() - vt0;
    if (vad) {
      fprintf(stderr, "[kws] silero vad loaded in %llums (model=%s thr=%.2f)\n",
              (unsigned long long)vload_ms, args.vad_model, args.vad_threshold);
    } else {
      fprintf(stderr,
              "[kws] silero vad load failed (model=%s); continuing kws-only\n",
              args.vad_model);
    }
  }

  fputs("{\"event\":\"ready\",\"name\":", stdout);
  print_json_str(args.name);
  printf(",\"load_ms\":%llu,"
         "\"threshold\":%.3f,\"score\":%.2f,\"trailing_blanks\":%d,"
         "\"threads\":%d,\"cpu\":%d,\"vad\":%s,\"vad_gain\":%.2f}\n",
         (unsigned long long)load_ms, args.threshold, args.score,
         args.trailing_blanks, args.num_threads, args.cpu,
         vad ? "true" : "false", args.vad_gain);
  fflush(stdout);
  fprintf(stderr, "[kws] ready in %llums (threshold=%.3f score=%.2f vad=%s gain=%.2f)\n",
          (unsigned long long)load_ms, args.threshold, args.score,
          vad ? "on" : "off", args.vad_gain);

  int16_t block[BLOCK_SAMPLES];
  float samples[BLOCK_SAMPLES];
  float vad_samples[BLOCK_SAMPLES];
  uint64_t fed = 0;
  uint64_t detections = 0;
  int last_vad = -1;
  uint64_t vad_buffer_samples = 0;
  uint64_t exit_ms = monotonic_ms();
  uint64_t pace_start = monotonic_ms();
  uint64_t last_feed_ms = monotonic_ms();  // when the newest sample reached us
  uint64_t pace_blocks = 0;

  while (!g_stop) {
    // --pace: read at real time. A file/replay feeder pushes blocks faster
    // than the model decodes, which would inflate `fed` at detection and
    // make the latency number meaningless. The firmware always feeds at
    // real time, so this flag is only for offline harnesses.
    if (args.pace) {
      uint64_t due = pace_start + (pace_blocks + 1) * (1000ull * BLOCK_SAMPLES / SAMPLE_RATE);
      uint64_t now = monotonic_ms();
      if (now < due) {
        struct timespec ts = {(time_t)((due - now) / 1000), (long)(((due - now) % 1000) * 1000000)};
        nanosleep(&ts, NULL);
      }
      pace_blocks++;
    }
    // Fill one full block (read() may return short); a short read at EOF is
    // a clean end of stream.
    //
    // Wait for readability in bounded slices instead of blocking in read():
    // with no PCM (muted device) a blocking read never returns, so g_stop
    // would never be seen and the firmware's deploy could not restart the
    // spotter. The stop is checked on every slice, including the return from
    // a signal-interrupted poll.
    size_t got = 0;
    while (got < sizeof(block)) {
      if (g_stop) break;
      int ready = wait_stdin(STOP_POLL_MS);
      if (ready == 0) continue;  // no audio in this window: re-check the stop flag
      if (ready < 0) {
        fprintf(stderr, "[kws] stdin poll failed: %s\n", strerror(errno));
        break;
      }
      ssize_t n = read(STDIN_FILENO, (char *)block + got, sizeof(block) - got);
      if (n < 0) {
        if (errno == EINTR) continue;  // a signal: the top of the loop decides
        fprintf(stderr, "[kws] stdin read failed: %s\n", strerror(errno));
        n = 0;
      }
      if (n == 0) break;
      got += (size_t)n;
    }
    if (g_stop || got == 0) break;
    size_t nsamp = got / sizeof(int16_t);
    for (size_t i = 0; i < nsamp; i++) samples[i] = (float)block[i] / 32768.0f;
    if (vad) {
      for (size_t i = 0; i < nsamp; i++) {
        float x = samples[i] * args.vad_gain;
        if (x > 1.0f) x = 1.0f;
        else if (x < -1.0f) x = -1.0f;
        vad_samples[i] = x;
      }
      SherpaOnnxVoiceActivityDetectorAcceptWaveform(vad, vad_samples, (int32_t)nsamp);
      int speech = SherpaOnnxVoiceActivityDetectorDetected(vad) ? 1 : 0;
      // We consume decisions, not accumulated audio. Flush before the 10 s
      // ring fills, preserving the detector state and this block's verdict.
      vad_buffer_samples += nsamp;
      if (vad_buffer_samples >= 5 * SAMPLE_RATE) {
        SherpaOnnxVoiceActivityDetectorFlush(vad);
        vad_buffer_samples = 0;
      }
      while (!SherpaOnnxVoiceActivityDetectorEmpty(vad)) {
        SherpaOnnxVoiceActivityDetectorPop(vad);
      }
      if (speech != last_vad) {
        printf("{\"event\":\"vad\",\"speech\":%s}\n", speech ? "true" : "false");
        fflush(stdout);
        last_vad = speech;
      }
    }
    SherpaOnnxOnlineStreamAcceptWaveform(stream, SAMPLE_RATE, samples, (int32_t)nsamp);
    fed += nsamp;
    last_feed_ms = monotonic_ms();

    while (!g_stop && SherpaOnnxIsKeywordStreamReady(spotter, stream)) {
      SherpaOnnxDecodeKeywordStream(spotter, stream);
      const SherpaOnnxKeywordResult *r = SherpaOnnxGetKeywordResult(spotter, stream);
      if (r && r->keyword && r->keyword[0] != '\0') {
        double last_ts = 0.0;   // phrase-relative; kept for the log only
        for (int32_t i = 0; i < r->count; i++) {
          if (r->timestamps && r->timestamps[i] > last_ts) last_ts = r->timestamps[i];
        }
        // Device-side freshness: how stale the newest audio was when the model
        // produced this result. Not the model's algorithmic delay (that is not
        // observable here — see the note at the top of this file).
        double fed_ms = (double)fed * 1000.0 / (double)SAMPLE_RATE;
        double lag_ms = (double)(monotonic_ms() - last_feed_ms);
        detections++;
        fputs("{\"event\":\"keyword\",\"keyword\":", stdout);
        print_json_str(r->keyword);
        printf(",\"fed_samples\":%llu,"
               "\"fed_ms\":%.0f,\"last_token_s\":%.3f,\"lag_ms\":%.0f,"
               "\"wall_ms\":%llu,\"n\":%llu}\n",
               (unsigned long long)fed, fed_ms, last_ts, lag_ms,
               (unsigned long long)monotonic_ms(), (unsigned long long)detections);
        fflush(stdout);
        fprintf(stderr, "[kws] HIT %s (fed=%.2fs lag=%.0fms)\n",
                r->keyword, fed_ms / 1000.0, lag_ms);
        // Reset so the same phrase can trigger again; the stream keeps its
        // feature pipeline, the decoder state restarts.
        SherpaOnnxResetKeywordStream(spotter, stream);
      }
      SherpaOnnxDestroyKeywordResult(r);
    }
  }

  printf("{\"event\":\"exit\",\"fed_samples\":%llu,\"detections\":%llu,"
         "\"uptime_ms\":%llu}\n",
         (unsigned long long)fed, (unsigned long long)detections,
         (unsigned long long)(monotonic_ms() - exit_ms));
  fflush(stdout);
  if (vad) SherpaOnnxDestroyVoiceActivityDetector(vad);
  SherpaOnnxDestroyOnlineStream(stream);
  SherpaOnnxDestroyKeywordSpotter(spotter);
  return 0;
}
