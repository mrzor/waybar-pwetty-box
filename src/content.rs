//! Tile content and where it comes from.
//!
//! A tile's content is a **Pango-markup string** (which may also contain custom
//! effect tags — see [`crate::markup`]). It's produced by a *source*:
//! - **static**: a fixed `text` value;
//! - **command**: the stdout of a shell command, re-run on an interval.
//!
//! The value is substituted into the `format` markup template (escaped, so
//! command output can't inject markup) and an optional `icon` glyph is
//! prepended. Commands run on a background thread and publish into a
//! [`ContentStore`], so a slow command never blocks the GTK main loop; the
//! widget polls the dirty flag and redraws when it flips.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::markup;

/// What a tile currently displays: a Pango-markup string and the float uniforms
/// (resolved from the data) for the background shader.
#[derive(Debug, Clone, Default)]
pub struct TileContent {
    pub markup: String,
    pub uniforms: Vec<(String, f32)>,
}

/// Thread-safe, cloneable handle to the current tile content.
#[derive(Clone)]
pub struct ContentStore {
    inner: Arc<Inner>,
}

struct Inner {
    content: Mutex<TileContent>,
    dirty: AtomicBool,
    /// Whether the current content has a *moving* element (blinking status,
    /// `<pulse>`, or a marquee) — i.e. whether it needs per-frame redraws. A
    /// static tile (idle/empty/plain wrapped text) is `false` and only repaints
    /// on a content change (the dirty flag), keeping the bar cool.
    animating: AtomicBool,
    /// Set once by [`ContentStore::shutdown`] (module teardown, e.g. a waybar
    /// reload). The poll/stream background loops and the dirty-poll timer
    /// check this each iteration and stop instead of running for the rest of
    /// the process.
    shutdown: AtomicBool,
    /// Pid of the currently-running stream child, 0 if none. Lets `shutdown`
    /// kill a live producer from another thread so a reader blocked on its
    /// stdout unblocks via EOF instead of leaking forever.
    child_pid: AtomicU32,
}

/// Whether rendered tile `markup` contains a continuously-animated element.
/// Idle/`empty`/`shell`-less static content is not animated; a blinking status
/// (`working`/`prompt`/`shell`), a `<pulse>`, or a `<tickerbox>` is.
pub fn content_animates(markup: &str) -> bool {
    // Quote-agnostic: templates may emit state='working' OR state="working".
    let has_state = |s: &str| {
        markup.contains(&format!("state='{s}'")) || markup.contains(&format!("state=\"{s}\""))
    };
    // A recently-idle indicator glows (slow pulse) through the first hour, then
    // goes static at the dimmest level (level 6) — so it animates unless it's 6.
    let idle_recent =
        has_state("idle") && !markup.contains("level='6'") && !markup.contains("level=\"6\"");
    idle_recent
        || has_state("working")
        || has_state("prompt")
        || has_state("shell")
        || markup.contains("<pulse")
        || markup.contains("<tickerbox")
        || markup.contains("<bg")
}

impl ContentStore {
    pub fn new(initial: TileContent) -> Self {
        let animating = content_animates(&initial.markup);
        Self {
            inner: Arc::new(Inner {
                content: Mutex::new(initial),
                dirty: AtomicBool::new(true),
                animating: AtomicBool::new(animating),
                shutdown: AtomicBool::new(false),
                child_pid: AtomicU32::new(0),
            }),
        }
    }

    /// Replace the content and mark it dirty (a redraw is due).
    pub fn set(&self, content: TileContent) {
        self.inner
            .animating
            .store(content_animates(&content.markup), Ordering::Release);
        if let Ok(mut guard) = self.inner.content.lock() {
            *guard = content;
        }
        self.inner.dirty.store(true, Ordering::Release);
    }

    /// Whether the current content animates (see [`content_animates`]).
    pub fn animating(&self) -> bool {
        self.inner.animating.load(Ordering::Acquire)
    }

    /// The current markup string (cheap clone for per-frame paint).
    pub fn markup(&self) -> String {
        self.inner
            .content
            .lock()
            .map(|g| g.markup.clone())
            .unwrap_or_default()
    }

    /// The current shader uniform values (resolved from the data).
    pub fn uniforms(&self) -> Vec<(String, f32)> {
        self.inner
            .content
            .lock()
            .map(|g| g.uniforms.clone())
            .unwrap_or_default()
    }

    /// Clear and return the dirty flag — true if content changed since last call.
    pub fn take_dirty(&self) -> bool {
        self.inner.dirty.swap(false, Ordering::AcqRel)
    }

    /// True once [`shutdown`](Self::shutdown) has been called. Checked by the
    /// poll/stream background loops and the dirty-poll timer.
    pub fn is_shutdown(&self) -> bool {
        self.inner.shutdown.load(Ordering::Acquire)
    }

    /// Tear down: stop the poll/stream loop (they check
    /// [`is_shutdown`](Self::is_shutdown)) and kill the current stream child,
    /// if any, so a reader thread blocked on its stdout unblocks via EOF
    /// instead of leaking a thread + producer chain forever. Call once, from
    /// module teardown (e.g. `PwettyBox`'s `Drop`).
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let pid = self.inner.child_pid.load(Ordering::Acquire);
        if pid != 0 {
            // SIGTERM via the `kill` binary rather than adding a libc/nix
            // dependency (neither is a direct dependency today) just to call
            // kill(2) directly.
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
    }

    /// Record the pid of the stream child currently being read, for
    /// [`shutdown`](Self::shutdown) to kill from another thread.
    fn set_child_pid(&self, pid: u32) {
        self.inner.child_pid.store(pid, Ordering::Release);
    }

    /// Clear the recorded child pid once it's no longer live.
    fn clear_child_pid(&self) {
        self.inner.child_pid.store(0, Ordering::Release);
    }
}

#[cfg(test)]
impl ContentStore {
    /// Test-only: the currently-recorded stream child pid (0 = none) — used
    /// to observe that the respawn loop stopped after `shutdown()`.
    pub(crate) fn child_pid(&self) -> u32 {
        self.inner.child_pid.load(Ordering::Acquire)
    }
}

/// How much larger than the base text an icon glyph is drawn.
const ICON_SCALE: f64 = 1.3;

/// Parse a command's output as JSON; if it isn't valid JSON, treat the whole
/// output as a string value (so plain-text commands still work via `{{ value }}`).
fn parse_data(output: &str) -> Value {
    serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.to_string()))
}

/// Parse a resolved uniform value: `true`/`false` → 1/0, otherwise a float
/// (unparseable → 0).
fn to_f32(s: &str) -> f32 {
    match s.trim() {
        "true" => 1.0,
        "false" => 0.0,
        other => other.parse().unwrap_or(0.0),
    }
}

/// Resolve each `shader_uniforms` template against the data into a float.
fn build_uniforms(spec: &HashMap<String, String>, data: &Value) -> Vec<(String, f32)> {
    spec.iter()
        .filter_map(|(name, tmpl)| {
            markup::render_template(tmpl, data)
                .ok()
                .map(|s| (name.clone(), to_f32(&s)))
        })
        .collect()
}

/// Build a tile's markup: render the `template` against `data`, then prepend the
/// `icon` glyph if set (sized + vertically centered; `base_px` = text size).
/// A template error is surfaced inline so it's visible on the bar.
fn build_markup(template: &str, icon: &Option<String>, data: &Value, base_px: f64) -> String {
    let body = match markup::render_template(template, data) {
        Ok(s) => s,
        Err(e) => format!(
            "<span foreground=\"#f38ba8\">template error: {}</span>",
            markup::escape(&e.to_string())
        ),
    };
    match icon {
        Some(i) if !i.is_empty() => {
            format!("{}  {body}", markup::icon_span(i, base_px, ICON_SCALE))
        }
        _ => body,
    }
}

/// Render a tile's markup for `config` against arbitrary `data` — the same
/// template + icon composition the live content path uses, but one-shot. Used by
/// the `pwetty` CLI to render a tile against a sample payload.
pub fn markup_for(config: &Config, data: &Value) -> String {
    let template = config.format.as_deref().unwrap_or("{{ value }}");
    build_markup(template, &config.icon, data, config.font_size as f64)
}

/// Build a [`ContentStore`] for the configured source, if any (`text`/`exec`).
/// For `exec`, spawns a background refresh thread. Returns `None` when no content
/// source is configured (the caller falls back to the demo tile).
pub fn from_config(config: &Config) -> Option<ContentStore> {
    let template = config
        .format
        .clone()
        .unwrap_or_else(|| "{{ value }}".to_string());
    let icon = config.icon.clone();
    let base_px = config.font_size as f64;
    let uniforms = config.shader_uniforms.clone().unwrap_or_default();

    if let Some(exec) = config.exec.clone() {
        let store = ContentStore::new(TileContent::default());
        let publish = store.clone();
        let build = ContentBuilder {
            template,
            icon,
            uniforms,
            base_px,
        };
        if config.stream {
            // Push mode: one long-lived process, content per stdout line.
            spawn_stream(exec, publish, build);
        } else {
            // Poll mode: re-run on the interval (0 = run once).
            let interval = config.interval;
            // Runs until `publish.shutdown()` is called (module teardown,
            // e.g. a waybar reload).
            thread::spawn(move || loop {
                if publish.is_shutdown() {
                    break;
                }
                // Known limit: a hung command blocks this thread inside
                // `Command::output()` with no timeout — shutdown only stops
                // the *next* iteration, not one already in flight. Add a
                // kill-on-timeout if a hung poll command bites.
                let data = parse_data(&run_command(&exec));
                publish.set(build.content(&data));
                if interval == 0 {
                    break;
                }
                thread::sleep(Duration::from_secs(interval));
            });
        }
        return Some(store);
    }

    config.text.as_deref().map(|text| {
        let build = ContentBuilder {
            template,
            icon,
            uniforms,
            base_px,
        };
        ContentStore::new(build.content(&parse_data(text)))
    })
}

/// The fixed inputs for turning a data [`Value`] into [`TileContent`] (the
/// `format` template, optional icon glyph, shader-uniform specs, base text size).
/// Lets the poll, stream, and static paths share one composition step.
struct ContentBuilder {
    template: String,
    icon: Option<String>,
    uniforms: HashMap<String, String>,
    base_px: f64,
}

impl ContentBuilder {
    fn content(&self, data: &Value) -> TileContent {
        TileContent {
            markup: build_markup(&self.template, &self.icon, data, self.base_px),
            uniforms: build_uniforms(&self.uniforms, data),
        }
    }
}

/// Streaming `exec`: spawn `cmd` **once** and apply each newline-delimited stdout
/// line as new content (push), instead of polling on an interval. On EOF/exit —
/// or on a decode error / an over-cap line — we keep the last content and
/// respawn after [`RESPAWN_BACKOFF`], so a producer crash *or* a framing
/// violation recovers, and a command that exits immediately can't busy-loop.
/// Blank lines are skipped; a non-JSON line is treated as a plain string value
/// (same as the poll path), so it never blanks the tile. Runs until
/// `publish.shutdown()` is called (module teardown, e.g. a waybar reload).
fn spawn_stream(cmd: String, publish: ContentStore, build: ContentBuilder) {
    /// Minimum delay before respawning an exited streaming command.
    const RESPAWN_BACKOFF: Duration = Duration::from_secs(1);
    /// Cap on a single stream line: a
    /// producer that never emits `\n` must not grow host memory unboundedly.
    /// ~120x the realistic tile-watch line (~550B); generous for real content,
    /// small enough to bound a runaway/hostile producer.
    const LINE_CAP: u64 = 64 * 1024;

    thread::spawn(move || loop {
        if publish.is_shutdown() {
            break;
        }
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                publish.set_child_pid(child.id());
                if let Some(out) = child.stdout.take() {
                    let mut reader = BufReader::new(out);
                    let mut buf = Vec::new();
                    loop {
                        buf.clear();
                        let n = match (&mut reader).take(LINE_CAP).read_until(b'\n', &mut buf) {
                            Ok(n) => n,
                            Err(_) => break, // I/O error: same recovery as a decode error
                        };
                        if n == 0 {
                            break; // EOF
                        }
                        let terminated = buf.last() == Some(&b'\n');
                        if !terminated && buf.len() as u64 >= LINE_CAP {
                            // Over-cap line, not newline-terminated within the
                            // cap: bail like a decode error rather than
                            // growing the buffer further.
                            break;
                        }
                        let Ok(text) = std::str::from_utf8(&buf) else {
                            break; // invalid UTF-8: same recovery as an over-cap line
                        };
                        let line = text.trim_end_matches(['\n', '\r']);
                        if !line.trim().is_empty() {
                            publish.set(build.content(&parse_data(line)));
                        }
                        if publish.is_shutdown() {
                            break;
                        }
                    }
                }
                // Kill before waiting: an EOF exit is already dying/dead (kill
                // is then a harmless no-op), but a decode-error/over-cap break
                // leaves a live, possibly-quiet child that would otherwise
                // gate `wait()` on its own next write.
                let _ = child.kill();
                let _ = child.wait();
                publish.clear_child_pid();
            }
            Err(e) => eprintln!("pwetty-box: cannot spawn stream exec '{cmd}': {e}"),
        }
        if publish.is_shutdown() {
            break;
        }
        // EOF / exit / spawn failure: keep last content, back off, respawn.
        thread::sleep(RESPAWN_BACKOFF);
    });
}

/// Run `cmd` via `sh -c` and return its trimmed stdout (empty on failure).
fn run_command(cmd: &str) -> String {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        build_markup, build_uniforms, content_animates, parse_data, ContentStore, TileContent,
    };

    #[test]
    fn content_animates_is_quote_agnostic() {
        // Templates emit single OR double quotes — both must be recognised, or
        // the frame clock won't animate the tile (the blink crawls to ~2 fps).
        assert!(content_animates("<status state='working' level='0'/>"));
        assert!(content_animates("<status state=\"shell\"/>"));
        assert!(content_animates("a <tickerbox>x</tickerbox>"));
        assert!(content_animates("<pulse>x</pulse>"));
        // Recently-idle glows (first hour); the dimmest level + plain content don't.
        assert!(content_animates("<status state='idle' level='2'/>"));
        assert!(!content_animates("<status state='idle' level='6'/>"));
        assert!(!content_animates("just text, folder named working"));
    }

    #[test]
    fn build_markup_binds_data_and_prefixes_icon() {
        use serde_json::json;
        // Scalar value bound via {{ value }}, escaped.
        assert_eq!(
            build_markup("{{ value }}", &None, &json!("a&b"), 30.0),
            "a&amp;b"
        );
        // Object field bound; literal markup preserved.
        assert_eq!(
            build_markup("<b>{{ n }}</b>", &None, &json!({ "n": 42 }), 30.0),
            "<b>42</b>"
        );
        // Icon glyph prepended as a (centered) span, body follows.
        let with_icon = build_markup("{{ value }}", &Some("I".into()), &json!("x"), 30.0);
        assert!(with_icon.contains("<span"), "icon wrapped: {with_icon}");
        assert!(with_icon.ends_with("x"), "body follows icon: {with_icon}");
        // Empty icon is ignored.
        assert_eq!(
            build_markup("{{ value }}", &Some(String::new()), &json!("x"), 30.0),
            "x"
        );
    }

    #[test]
    fn parse_data_json_or_plain() {
        assert!(parse_data(r#"{"a":1}"#).is_object());
        assert_eq!(
            parse_data("hello"),
            serde_json::Value::String("hello".into())
        );
    }

    #[test]
    fn store_tracks_dirty() {
        let s = ContentStore::new(TileContent::default());
        assert!(s.take_dirty(), "new store starts dirty");
        assert!(!s.take_dirty(), "cleared after read");
        s.set(TileContent {
            markup: "x".into(),
            ..Default::default()
        });
        assert!(s.take_dirty(), "dirty again after set");
        assert_eq!(s.markup(), "x");
    }

    #[test]
    fn build_uniforms_resolves_floats() {
        use serde_json::json;
        use std::collections::HashMap;
        let mut spec = HashMap::new();
        spec.insert("u_load".to_string(), "{{ pct }}".to_string());
        spec.insert("u_hot".to_string(), "{{ pct >= 90 }}".to_string());
        let u = build_uniforms(&spec, &json!({ "pct": 95 }));
        let get = |n: &str| u.iter().find(|(k, _)| k == n).map(|(_, v)| *v);
        assert_eq!(get("u_load"), Some(95.0));
        assert_eq!(get("u_hot"), Some(1.0)); // bool true -> 1.0
    }

    #[test]
    fn stream_exec_pushes_each_line_as_content() {
        use crate::config::Config;
        use std::time::{Duration, Instant};
        // A streaming command that emits two JSON lines then exits. Each line
        // should become tile content as it arrives (push), not after an interval.
        let store = super::from_config(&Config {
            exec: Some(r#"printf '{"value":"one"}\n{"value":"two"}\n'"#.into()),
            stream: true,
            format: Some("{{ value }}".into()),
            font_size: 14.0,
            ..Default::default()
        })
        .expect("exec source yields a store");

        // Poll the dirty-flag content until the last line lands (or time out).
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.markup() != "two" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(store.markup(), "two", "stream applied the latest line");
    }

    #[test]
    fn shutdown_stops_respawn_loop() {
        use crate::config::Config;
        use std::time::{Duration, Instant};
        // A short-lived producer, so the respawn loop cycles quickly.
        let store = super::from_config(&Config {
            exec: Some("printf 'x\\n'; sleep 0.05".into()),
            stream: true,
            format: Some("{{ value }}".into()),
            font_size: 14.0,
            ..Default::default()
        })
        .expect("exec source yields a store");

        // Confirm the chain is actually running before tearing it down.
        let deadline = Instant::now() + Duration::from_secs(2);
        while store.child_pid() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            store.child_pid(),
            0,
            "producer chain running before shutdown"
        );

        store.shutdown();

        // Let any in-flight iteration see the flag and unwind (kill + wait).
        std::thread::sleep(Duration::from_millis(200));
        // RESPAWN_BACKOFF is 1s; poll well past it and confirm the pid never
        // comes back — a respawn would flip it nonzero again.
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            assert_eq!(store.child_pid(), 0, "respawned after shutdown");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn stream_recovers_after_invalid_utf8_line() {
        use crate::config::Config;
        use std::time::{Duration, Instant};
        // First invocation emits one invalid-UTF-8 line then goes quiet
        // forever (sleep) — at HEAD this wedges the reader in `child.wait()`
        // with no respawn, since the child never writes again. The fix kills
        // the child on the decode-error path instead, so the respawn loop
        // spawns a second invocation; the marker file makes that one emit
        // valid content instead of repeating the bad line, so recovery is
        // directly observable. (dash's builtin `printf` has no `\xHH`
        // escape — `\377` is the portable octal form for a raw 0xFF byte.)
        let marker =
            std::env::temp_dir().join(format!("pwetty-box-decode-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker); // clear a stale marker, if any
        let cmd = format!(
            "if [ -f {m} ]; then printf '{{\"value\":\"recovered\"}}\\n'; sleep 30; \
             else touch {m}; printf '\\377\\377\\n'; sleep 30; fi",
            m = marker.display()
        );
        let store = super::from_config(&Config {
            exec: Some(cmd),
            stream: true,
            format: Some("{{ value }}".into()),
            font_size: 14.0,
            ..Default::default()
        })
        .expect("exec source yields a store");

        let deadline = Instant::now() + Duration::from_secs(5);
        while store.markup() != "recovered" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            store.markup(),
            "recovered",
            "stream recovered after a decode-error line"
        );
        store.shutdown(); // reap the still-running "sleep 30" child
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn stream_recovers_after_over_cap_line() {
        use crate::config::Config;
        use std::time::{Duration, Instant};
        // First invocation emits a line far over LINE_CAP (64 KiB) with no
        // terminator, then goes quiet; the marker makes the respawned second
        // invocation emit a normal valid line instead, so we can observe
        // recovery (rather than an unbounded buffer / stuck reader).
        let marker =
            std::env::temp_dir().join(format!("pwetty-box-cap-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let cmd = format!(
            "if [ -f {m} ]; then printf '{{\"value\":\"after-cap\"}}\\n'; sleep 30; \
             else touch {m}; head -c 70000 /dev/zero | tr '\\0' 'A'; sleep 30; fi",
            m = marker.display()
        );
        let store = super::from_config(&Config {
            exec: Some(cmd),
            stream: true,
            format: Some("{{ value }}".into()),
            font_size: 14.0,
            ..Default::default()
        })
        .expect("exec source yields a store");

        let deadline = Instant::now() + Duration::from_secs(6);
        while store.markup() != "after-cap" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            store.markup(),
            "after-cap",
            "valid line landed after an over-cap line"
        );
        store.shutdown(); // reap the still-running "sleep 30" child
        let _ = std::fs::remove_file(&marker);
    }
}
