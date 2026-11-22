use super::*;

/// Contract v2 §2.5 (#5, 2026-11-16): the same deploy carries the words whose
/// intent is `retreat` and a version of the word→intent map. Those are kept
/// in `kw.exit.json` next to the vocabulary so the spotter's reader can act
/// on a hit WITHOUT this core. The set is only ever a subset of the file:
/// a word that is not in the vocabulary can never be heard, so it is dropped
/// (and named in the detail) rather than armed.
///
pub(super) fn validate_keywords_only(dir: &std::path::Path, keywords: &str) -> Result<u64, String> {
    let tokens_path = dir.join("model").join("tokens.txt");
    let raw = std::fs::read_to_string(&tokens_path)
        .map_err(|_| format!("model token table missing: {}", tokens_path.display()))?;
    let known: std::collections::HashSet<&str> = raw
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    let mut lines = 0u64;
    for (i, line) in keywords.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (tok, text) = match line.split_once('@') {
            Some((t, x)) if !x.trim().is_empty() => (t.trim(), x.trim()),
            _ => return Err(format!("line {}: expected '<tokens> @<text>'", i + 1)),
        };
        if tok.split_whitespace().count() == 0 {
            return Err(format!("line {}: no tokens for '{text}'", i + 1));
        }
        for t in tok.split_whitespace() {
            if !known.contains(t) {
                return Err(format!(
                    "line {}: unknown token '{t}' in '{text}' (not in model/tokens.txt)",
                    i + 1
                ));
            }
        }
        lines += 1;
    }
    if lines == 0 {
        return Err("no keywords in the file".into());
    }
    Ok(lines)
}

/// Returns (accepted, hash, keyword lines, detail, armed local-exit words).
pub(super) fn deploy_keywords(
    dir: &std::path::Path,
    keywords: &str,
    retreat_words: &[String],
    intents_hash: &str,
) -> (bool, String, u64, String, u32) {
    let tokens_path = dir.join("model").join("tokens.txt");
    let Ok(raw) = std::fs::read_to_string(&tokens_path) else {
        return (
            false,
            String::new(),
            0,
            format!("model token table missing: {}", tokens_path.display()),
            0,
        );
    };
    let known: std::collections::HashSet<&str> = raw
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    let mut lines = 0u64;
    let mut out = String::new();
    let mut texts: Vec<String> = Vec::new();
    for (i, line) in keywords.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (tok, text) = match line.split_once('@') {
            Some((t, x)) if !x.trim().is_empty() => (t.trim(), x.trim()),
            _ => {
                return (
                    false,
                    String::new(),
                    0,
                    format!("line {}: expected '<tokens> @<text>'", i + 1),
                    0,
                )
            }
        };
        for t in tok.split_whitespace() {
            if !known.contains(t) {
                return (
                    false,
                    String::new(),
                    0,
                    format!(
                        "line {}: unknown token '{t}' in '{text}' (not in model/tokens.txt)",
                        i + 1
                    ),
                    0,
                );
            }
        }
        if tok.split_whitespace().count() == 0 {
            return (
                false,
                String::new(),
                0,
                format!("line {}: no tokens for '{text}'", i + 1),
                0,
            );
        }
        out.push_str(tok);
        out.push(' ');
        out.push('@');
        out.push_str(text);
        out.push('\n');
        texts.push(text.to_string());
        lines += 1;
    }
    if lines == 0 {
        return (false, String::new(), 0, "no keywords in the file".into(), 0);
    }
    // The local-exit set: retreat words that ARE in the vocabulary. A retreat
    // word missing from the file is dropped and named — never armed on hope.
    let mut exit_words: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for w in retreat_words {
        let w = w.trim();
        if w.is_empty() {
            continue;
        }
        if !texts.iter().any(|t| t == w) {
            if !dropped.iter().any(|d| d == w) {
                dropped.push(w.to_string());
            }
            continue;
        }
        if !exit_words.iter().any(|e| e == w) {
            exit_words.push(w.to_string());
        }
    }
    let hash = fnv1a_hex(out.as_bytes());
    let kw_path = dir.join("kw.txt");
    // Back up the previous vocabulary (one generation is enough: the console
    // keeps the config history and can always recompile).
    if let Ok(prev) = std::fs::read(&kw_path) {
        let _ = std::fs::write(dir.join("kw.txt.bak"), prev);
    }
    let tmp = dir.join("kw.txt.tmp");
    if let Err(e) = std::fs::write(&tmp, out.as_bytes()) {
        return (
            false,
            String::new(),
            0,
            format!("write {}: {e}", tmp.display()),
            0,
        );
    }
    if let Err(e) = std::fs::rename(&tmp, &kw_path) {
        return (
            false,
            String::new(),
            0,
            format!("install {}: {e}", kw_path.display()),
            0,
        );
    }
    // Install the meaning AFTER the vocabulary: a reader that sees the new
    // set always finds the words it names in kw.txt.
    let exit_path = dir.join("kw.exit.json");
    let armed = exit_words.len() as u32;
    let installed = if exit_words.is_empty() {
        // No retreat word (or an older core): drop any previous set so a
        // stale word cannot cut the audio after this deploy.
        let _ = std::fs::remove_file(&exit_path);
        String::new()
    } else {
        let doc = serde_json::json!({ "hash": intents_hash, "words": exit_words });
        let tmp = dir.join("kw.exit.json.tmp");
        match std::fs::write(&tmp, doc.to_string()) {
            Ok(()) => match std::fs::rename(&tmp, &exit_path) {
                Ok(()) => String::new(),
                Err(e) => format!("; exit set not installed: {e}"),
            },
            Err(e) => format!("; exit set not installed: {e}"),
        }
    };
    let dropped_note = if dropped.is_empty() {
        String::new()
    } else {
        format!("; not in the vocabulary, not armed: {}", dropped.join(", "))
    };
    (
        true,
        hash,
        lines,
        format!(
            "{lines} keywords installed at {}; {armed} local exit word(s){installed}{dropped_note}",
            kw_path.display()
        ),
        armed,
    )
}
