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
// 2.5 s PCM ring buffer (160 KB) for XiaoAi-style L1 -> L2 isolated-slice
// acoustic verification (#69). Full 320 ms tail padding flushes Zipformer chunk-16.
#define RING_SAMPLES (SAMPLE_RATE * 5 / 2)
#define L2_TAIL_PAD_SAMPLES 5120  // 320 ms (1 full chunk-16)
#define MAX_KEYWORDS 64

static float g_ring[RING_SAMPLES];
static size_t g_ring_pos = 0;
static uint64_t g_ring_total = 0;
static float g_l2_buf[RING_SAMPLES + L2_TAIL_PAD_SAMPLES];

static void ring_push(const float *samples, size_t n) {
  for (size_t i = 0; i < n; i++) {
    g_ring[g_ring_pos] = samples[i];
    g_ring_pos = (g_ring_pos + 1) % RING_SAMPLES;
  }
  g_ring_total += n;
}

static size_t ring_copy_tail(float *dst, size_t want) {
  uint64_t avail = g_ring_total < RING_SAMPLES ? g_ring_total : RING_SAMPLES;
  size_t n = want < (size_t)avail ? want : (size_t)avail;
  size_t start = (g_ring_pos + RING_SAMPLES - n) % RING_SAMPLES;
  if (start + n <= RING_SAMPLES) {
    memcpy(dst, g_ring + start, n * sizeof(float));
  } else {
    size_t first = RING_SAMPLES - start;
    memcpy(dst, g_ring + start, first * sizeof(float));
    memcpy(dst + first, g_ring, (n - first) * sizeof(float));
  }
  return n;
}

struct KwEntry {
  char tokens[256];
  char label[128];
  int token_count;
};

static char *trim_ws(char *s) {
  while (*s == ' ' || *s == '\t' || *s == '\r' || *s == '\n') s++;
  size_t len = strlen(s);
  while (len > 0 && (s[len - 1] == ' ' || s[len - 1] == '\t' || s[len - 1] == '\r' ||
                     s[len - 1] == '\n')) {
    s[--len] = '\0';
  }
  return s;
}

// Parse kw.txt into per-keyword token spellings and build an explicit L1
// keyword buffer (`<tokens> :<l1_score> #<l1_threshold> @<label>\n`) so a
// single SherpaOnnxKeywordSpotter can run both L1 and isolated-slice L2 streams
// without merging conflicting thresholds into the same trie (#69).
static int load_keywords_table(const char *path, struct KwEntry *out, int max_out,
                               char *l1_buf, size_t l1_buf_sz,
                               float l1_score, float l1_threshold) {
  FILE *fp = fopen(path, "r");
  if (!fp) return -1;
  char line[512];
  int count = 0;
  size_t used = 0;
  if (l1_buf && l1_buf_sz > 0) l1_buf[0] = '\0';

  while (fgets(line, sizeof(line), fp) && count < max_out) {
    char *s = trim_ws(line);
    if (s[0] == '\0' || s[0] == '#') continue;
    char *at = strchr(s, '@');
    if (!at) continue;
    *at = '\0';
    char *lbl = trim_ws(at + 1);
    char *toks_raw = trim_ws(s);
    if (lbl[0] == '\0' || toks_raw[0] == '\0') continue;

    // Strip any inline :score / #threshold modifiers from the raw token list.
    char clean_toks[256];
    size_t ct_used = 0;
    clean_toks[0] = '\0';
    int ntok = 0;
    char *p = toks_raw;
    while (*p) {
      while (*p == ' ' || *p == '\t') p++;
      if (!*p) break;
      char *start = p;
      while (*p && *p != ' ' && *p != '\t') p++;
      size_t tlen = (size_t)(p - start);
      if (tlen > 0 && start[0] != ':' && start[0] != '#') {
        if (ct_used + (ct_used > 0 ? 1 : 0) + tlen + 1 < sizeof(clean_toks)) {
          if (ct_used > 0) clean_toks[ct_used++] = ' ';
          memcpy(clean_toks + ct_used, start, tlen);
          ct_used += tlen;
          clean_toks[ct_used] = '\0';
          ntok++;
        }
      }
    }
    if (ntok == 0) continue;

    snprintf(out[count].tokens, sizeof(out[count].tokens), "%s", clean_toks);
    snprintf(out[count].label, sizeof(out[count].label), "%s", lbl);
    out[count].token_count = ntok;
    if (l1_buf && used + 1 < l1_buf_sz) {
      int w = snprintf(l1_buf + used, l1_buf_sz - used, "%s :%.2f #%.3f @%s\n",
                       clean_toks, l1_score, l1_threshold, lbl);
      if (w > 0 && (size_t)w < l1_buf_sz - used) used += (size_t)w;
    }
    count++;
  }
  fclose(fp);
  return count;
}

static const struct KwEntry *find_kw(const struct KwEntry *entries, int n, const char *label) {
  if (!label) return NULL;
  for (int i = 0; i < n; i++) {
    if (strcmp(entries[i].label, label) == 0) return &entries[i];
  }
  return NULL;
}

static int run_l2_slice_verify(const SherpaOnnxKeywordSpotter *spotter,
                               const struct KwEntry *kw,
                               float l2_score, float l2_threshold,
                               double wake_dur_ms) {
  char l2_line[512];
  snprintf(l2_line, sizeof(l2_line), "%s :%.2f #%.3f @%s\n",
           kw->tokens, l2_score, l2_threshold, kw->label);
  const SherpaOnnxOnlineStream *l2 = SherpaOnnxCreateKeywordStreamWithKeywords(spotter, l2_line);
  if (!l2) return 0;

  // Slice [wake_start - context .. wake_end + chunk_latency] from the 2.5 s ring
  // buffer (clamped to [1500 ms, 2200 ms]) plus 320 ms zero tail so Zipformer
  // chunk-16 buffering phase delay (~400-760 ms, especially on tight oneshots)
  // never clips the opening syllable of the wake word.
  double want_ms = wake_dur_ms + 900.0;
  if (want_ms < 1500.0) want_ms = 1500.0;
  if (want_ms > 2200.0) want_ms = 2200.0;
  size_t want_samples = (size_t)(want_ms * (double)(SAMPLE_RATE / 1000));
  size_t got = ring_copy_tail(g_l2_buf, want_samples);
  memset(g_l2_buf + got, 0, L2_TAIL_PAD_SAMPLES * sizeof(float));

  SherpaOnnxOnlineStreamAcceptWaveform(l2, SAMPLE_RATE, g_l2_buf,
                                       (int32_t)(got + L2_TAIL_PAD_SAMPLES));
  SherpaOnnxOnlineStreamInputFinished(l2);
  int matched = 0;
  while (SherpaOnnxIsKeywordStreamReady(spotter, l2)) {
    SherpaOnnxDecodeKeywordStream(spotter, l2);
    const SherpaOnnxKeywordResult *lr = SherpaOnnxGetKeywordResult(spotter, l2);
    if (lr && lr->keyword && strcmp(lr->keyword, kw->label) == 0) {
      matched = 1;
      SherpaOnnxDestroyKeywordResult(lr);
      break;
    }
    SherpaOnnxDestroyKeywordResult(lr);
  }
  SherpaOnnxDestroyOnlineStream(l2);
  return matched;
}

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
          "          [--l2-threshold 0.0] [--l2-score 2.20]\n"
          "          [--min-wake-ms 400] [--max-wake-ms 1400]\n"
          "          [--max-token-gap-ms 380] [--l2-min-tokens 7]\n"
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
  float l2_threshold, l2_score;
  int min_wake_ms, max_wake_ms, max_token_gap_ms, l2_min_tokens;
  float vad_threshold, vad_gain, vad_min_silence, vad_min_speech;
  int trailing_blanks, num_threads, cpu, vad_window_size;
  int pace;  // read at real time (test harnesses pipe faster than the model)
};

static int parse_args(int argc, char **argv, struct Args *a) {
  memset(a, 0, sizeof(*a));
  a->threshold = 0.07f;  // RUI production value (spike: 5/5 real speech)
  a->score = 4.0f;       // RUI production boost for the keyword line
  a->l2_threshold = 0.0f;   // 0 = disabled (legacy L1-only compatibility)
  a->l2_score = 2.20f;
  a->min_wake_ms = 400;
  a->max_wake_ms = 1400;
  a->max_token_gap_ms = 380;
  a->l2_min_tokens = 7;
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
    else if (!strcmp(k, "--l2-threshold")) a->l2_threshold = strtof(v, NULL);
    else if (!strcmp(k, "--l2-score")) a->l2_score = strtof(v, NULL);
    else if (!strcmp(k, "--min-wake-ms")) a->min_wake_ms = (int)strtol(v, NULL, 10);
    else if (!strcmp(k, "--max-wake-ms")) a->max_wake_ms = (int)strtol(v, NULL, 10);
    else if (!strcmp(k, "--max-token-gap-ms")) a->max_token_gap_ms = (int)strtol(v, NULL, 10);
    else if (!strcmp(k, "--l2-min-tokens")) a->l2_min_tokens = (int)strtol(v, NULL, 10);
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

  struct KwEntry kw_entries[MAX_KEYWORDS];
  char l1_buf[16384];
  int kw_count = load_keywords_table(args.keywords_file, kw_entries, MAX_KEYWORDS,
                                     l1_buf, sizeof(l1_buf), args.score, args.threshold);
  int l2_enabled = args.l2_threshold > 0.0f;
  if (l2_enabled && kw_count <= 0) {
    fprintf(stderr, "[kws] failed to parse keywords file %s for L1+L2\n", args.keywords_file);
    printf("{\"event\":\"error\",\"stage\":\"keywords\"}\n");
    fflush(stdout);
    return 2;
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
  if (l2_enabled) {
    // Keep the spotter's global keyword trie empty so CreateKeywordStreamWithKeywords
    // does not merge L1 global entries into an isolated L2 verification stream (#69).
    config.keywords_file = NULL;
    config.keywords_buf = "\n";
    config.keywords_buf_size = 1;
  } else {
    config.keywords_file = args.keywords_file;
  }

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
  const SherpaOnnxOnlineStream *stream = l2_enabled
      ? SherpaOnnxCreateKeywordStreamWithKeywords(spotter, l1_buf)
      : SherpaOnnxCreateKeywordStream(spotter);
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
         "\"threshold\":%.3f,\"score\":%.2f,\"l2_threshold\":%.3f,\"l2_score\":%.2f,"
         "\"trailing_blanks\":%d,\"threads\":%d,\"cpu\":%d,\"vad\":%s,\"vad_gain\":%.2f}\n",
         (unsigned long long)load_ms, args.threshold, args.score,
         args.l2_threshold, args.l2_score,
         args.trailing_blanks, args.num_threads, args.cpu,
         vad ? "true" : "false", args.vad_gain);
  fflush(stdout);
  fprintf(stderr,
          "[kws] ready in %llums (threshold=%.3f score=%.2f l2_threshold=%.3f l2_score=%.2f vad=%s gain=%.2f)\n",
          (unsigned long long)load_ms, args.threshold, args.score,
          args.l2_threshold, args.l2_score,
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
    ring_push(samples, nsamp);
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
        double first_ts = -1.0;
        double last_ts = 0.0;   // phrase-relative; kept for telemetry & log
        double max_gap_s = 0.0;
        double prev_ts = -1.0;
        for (int32_t i = 0; i < r->count; i++) {
          if (r->timestamps) {
            double ts = r->timestamps[i];
            if (first_ts < 0.0 || ts < first_ts) first_ts = ts;
            if (ts > last_ts) last_ts = ts;
            if (prev_ts >= 0.0) {
              double gap = ts - prev_ts;
              if (gap > max_gap_s) max_gap_s = gap;
            }
            prev_ts = ts;
          }
        }
        double wake_dur_ms = (first_ts >= 0.0 && last_ts >= first_ts)
            ? (last_ts - first_ts) * 1000.0 + 80.0
            : 0.0;
        double max_gap_ms = max_gap_s * 1000.0;
        unsigned long long wake_dur_u64 = (unsigned long long)(wake_dur_ms + 0.5);
        unsigned long long max_gap_u64 = (unsigned long long)(max_gap_ms + 0.5);

        const struct KwEntry *kw = find_kw(kw_entries, kw_count, r->keyword);
        int check_wake_tier = kw && kw->token_count >= args.l2_min_tokens;
        int l2_passed = 0;
        uint64_t l2_ms = 0;
        const char *tier = NULL;
        if (check_wake_tier) {
          int dur_ok = wake_dur_ms >= (double)args.min_wake_ms &&
                       wake_dur_ms <= (double)args.max_wake_ms &&
                       max_gap_ms <= (double)args.max_token_gap_ms;
          if (l2_enabled) {
            uint64_t l2_t0 = monotonic_ms();
            l2_passed = run_l2_slice_verify(spotter, kw, args.l2_score,
                                            args.l2_threshold, wake_dur_ms);
            l2_ms = monotonic_ms() - l2_t0;
            tier = (dur_ok && l2_passed) ? "real" : "suspect";
          } else {
            // Frame-duration + token-gap acoustic check without duplicating
            // the same 3.3M Zipformer encoder (~480 ms single-core penalty).
            l2_passed = dur_ok;
            tier = dur_ok ? "real" : "suspect";
          }
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
               "\"wake_dur_ms\":%llu,\"max_gap_ms\":%llu",
               (unsigned long long)fed, fed_ms, last_ts, lag_ms,
               wake_dur_u64, max_gap_u64);
        if (tier) {
          printf(",\"tier\":\"%s\",\"l2_passed\":%s,\"l2_ms\":%llu",
                 tier, l2_passed ? "true" : "false", (unsigned long long)l2_ms);
        }
        printf(",\"wall_ms\":%llu,\"n\":%llu}\n",
               (unsigned long long)monotonic_ms(), (unsigned long long)detections);
        fflush(stdout);
        if (tier) {
          fprintf(stderr,
                  "[kws] HIT %s (fed=%.2fs lag=%.0fms tier=%s dur=%llums gap=%llums l2=%s %llums)\n",
                  r->keyword, fed_ms / 1000.0, lag_ms, tier, wake_dur_u64,
                  max_gap_u64, l2_passed ? "pass" : "fail", (unsigned long long)l2_ms);
        } else {
          fprintf(stderr, "[kws] HIT %s (fed=%.2fs lag=%.0fms dur=%llums gap=%llums)\n",
                  r->keyword, fed_ms / 1000.0, lag_ms, wake_dur_u64, max_gap_u64);
        }
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
