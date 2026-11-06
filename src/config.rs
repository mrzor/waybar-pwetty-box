//! Module configuration, deserialized from the waybar `cffi/...` block.
//!
//! Waybar passes each JSON value of the module's config object to the module as
//! string key/value pairs; `waybar-cffi` collects them and hands us a
//! `serde`-deserializable struct via the `Module::Config` associated type.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Logical width of the tile area, in pixels (before HiDPI scaling).
    pub width: i32,
    /// Logical height of the tile area, in pixels (before HiDPI scaling).
    pub height: i32,
    /// Target animation framerate. The render loop ticks at this rate via the
    /// GTK frame clock; set to 0 to render once and stay static.
    pub fps: u32,
    /// Path to the primary text font (TTF/OTF). If unset or unloadable we fall
    /// back to a list of common system fonts.
    pub font_path: Option<String>,
    /// Path to an icon font (e.g. a Nerd Font) used for glyph icons in tiles.
    pub icon_font_path: Option<String>,
    /// Base font size in pixels.
    pub font_size: f32,
    /// Pango font family for tile text (e.g. `"Terminus"`, `"monospace"`). A
    /// comma-separated list is allowed (Pango picks the first available). When
    /// unset, falls back to `"sans"`.
    pub font_family: Option<String>,
    /// Horizontal alignment of inline-embed flow content: `"center"` centers each
    /// line within the tile width (for compact tiles); anything else (default)
    /// left-aligns with a small pad.
    pub align: Option<String>,
    /// Background cleared behind tiles, as a hex color (`#rrggbb` or
    /// `#rrggbbaa`). Omit for a transparent tile (the bar shows through — the
    /// Cairo composite honors per-pixel alpha); set it for an opaque tile.
    pub background: Option<String>,
    /// Corner radius of the focus bubble (the active-desktop card and the
    /// `<bg>` shader mask), in logical px. Unset defaults to 20% of the bubble
    /// height; `0` gives square corners.
    pub corner_radius: Option<f64>,

    // --- content source (see `crate::content`) ---
    /// Static tile text. Supports `\n` for multiline. Ignored if `exec` is set.
    pub text: Option<String>,
    /// Shell command whose stdout becomes the tile text. Re-run every
    /// `interval` seconds.
    pub exec: Option<String>,
    /// Re-run cadence for `exec`, in seconds. `0` runs it once.
    pub interval: u64,
    /// Streaming `exec` mode (push, not poll). When `true`, the command is
    /// spawned **once** and each newline-delimited stdout line becomes new tile
    /// content immediately (sub-150ms repaint), instead of re-running every
    /// `interval` seconds. `interval` is ignored. On EOF/exit the last content is
    /// kept and the command respawns after a short backoff. Default `false`.
    pub stream: bool,
    /// Static icon glyph (e.g. a Nerd Font character), drawn before the text.
    pub icon: Option<String>,
    /// Tile template (minijinja) rendered against the data into Pango markup.
    /// Defaults to `"{{ value }}"`.
    pub format: Option<String>,
    /// Path to a Shadertoy-style GLSL fragment shader (defining `mainImage`)
    /// used as the tile's animated background. Reloaded when the file changes.
    pub background_shader: Option<String>,
    /// Float uniforms fed to `background_shader` from the data: each value is a
    /// template (e.g. `"{{ cpu.pct }}"`) evaluated against the tile data and
    /// parsed as a float (`true`/`false` → 1/0). Declare matching
    /// `uniform float <name>;` in the shader.
    pub shader_uniforms: Option<HashMap<String, String>>,

    // --- drag-and-drop between tiles (see `lib.rs`) ---
    /// Identity of this tile for drag-and-drop. When set, the tile can be
    /// dragged onto any other tile of the same waybar that also sets one (and
    /// receives their drops); the id travels as the drag payload.
    #[serde(deserialize_with = "scalar_string")]
    pub drag_id: Option<String>,
    /// Shell command run on the tile a drag lands **on**, with `{src}` and
    /// `{dst}` replaced by the dragged and the receiving tile's `drag_id`.
    /// E.g. `"claude-status tile-move {src} {dst}"`.
    pub on_drop: Option<String>,
}

/// Waybar hands the module a string value that looks like a number as that
/// number (`"drag_id": "3"` arrives as `3`), so accept any scalar as a string.
fn scalar_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.map(|v| match v {
        Value::String(s) => s,
        other => other.to_string(),
    }))
}

/// Deep-merge `over` into `base`: two objects merge key-by-key (recursively);
/// any other value in `over` (scalar, array, null) replaces `base` wholesale.
/// Used to layer a bundled tile preset (base) under a waybar module config
/// (over) so the module's own keys win.
pub fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// The preset JSON to layer under `raw`, if it names one. `tile_file` (an
/// external path) takes precedence over `tile` (a bundled preset name). A bad
/// path / unknown name / malformed preset logs and yields `None` (so the raw
/// config is used as-is).
fn preset_for(raw: &Value) -> Option<Value> {
    if let Some(path) = raw.get("tile_file").and_then(Value::as_str) {
        return match std::fs::read_to_string(path) {
            Ok(s) => parse_preset(&s, path),
            Err(e) => {
                eprintln!("pwetty-box: cannot read tile_file '{path}': {e}");
                None
            }
        };
    }
    if let Some(name) = raw.get("tile").and_then(Value::as_str) {
        return match crate::tiles::get(name) {
            Some(p) => parse_preset(p.config, name),
            None => {
                eprintln!("pwetty-box: unknown tile preset '{name}'");
                None
            }
        };
    }
    None
}

fn parse_preset(src: &str, label: &str) -> Option<Value> {
    match serde_json::from_str(src) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("pwetty-box: invalid tile preset '{label}': {e}");
            None
        }
    }
}

/// Resolve a raw waybar module config (JSON `Value`) into a typed [`Config`],
/// layering a bundled/file tile preset underneath when `tile`/`tile_file` is
/// set (the module's own keys win). Unknown keys (`tile`, `tile_file`) are
/// ignored by the typed deserialize. On a hard deserialize error, logs and
/// falls back to defaults rather than panicking inside the CFFI boundary.
pub fn resolve(raw: Value) -> Config {
    let merged = match preset_for(&raw) {
        Some(mut base) => {
            merge(&mut base, &raw);
            base
        }
        None => raw,
    };
    serde_json::from_value(merged).unwrap_or_else(|e| {
        eprintln!("pwetty-box: invalid config: {e}");
        Config::default()
    })
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 220,
            height: 36,
            fps: 60,
            font_path: None,
            icon_font_path: None,
            font_size: 14.0,
            background: None,
            corner_radius: None,
            font_family: None,
            align: None,
            text: None,
            exec: None,
            interval: 0,
            stream: false,
            icon: None,
            format: None,
            background_shader: None,
            shader_uniforms: None,
            drag_id: None,
            on_drop: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{merge, resolve, Config};
    use serde_json::json;

    #[test]
    fn merge_object_keys_recursively_over_wins() {
        let mut base = json!({ "a": 1, "b": { "x": 1, "y": 2 }, "c": 3 });
        merge(&mut base, &json!({ "b": { "y": 20, "z": 30 }, "c": 99 }));
        assert_eq!(
            base,
            json!({ "a": 1, "b": { "x": 1, "y": 20, "z": 30 }, "c": 99 })
        );
    }

    #[test]
    fn merge_scalar_and_array_replace_not_merge() {
        let mut base = json!({ "arr": [1, 2, 3], "s": "old" });
        merge(&mut base, &json!({ "arr": [9], "s": "new" }));
        assert_eq!(base, json!({ "arr": [9], "s": "new" }));
    }

    #[test]
    fn resolve_layers_bundled_preset_under_module_keys() {
        // `tile: claude` brings the preset geometry + a format; the module
        // overrides width and adds exec. Module keys win; preset fills the rest.
        let c = resolve(json!({ "tile": "claude", "width": 360, "exec": "echo" }));
        assert_eq!(c.width, 360); // module override wins
        assert_eq!(c.height, 96); // from preset
        assert_eq!(c.exec.as_deref(), Some("echo"));
        assert!(c.format.as_deref().unwrap().contains("<status"));
    }

    #[test]
    fn resolve_without_tile_is_plain_config() {
        let c = resolve(json!({ "width": 123 }));
        assert_eq!(c.width, 123);
        assert_eq!(c.height, 36); // default
    }

    #[test]
    fn resolve_unknown_tile_falls_back_to_raw() {
        let c = resolve(json!({ "tile": "ghost", "width": 77 }));
        assert_eq!(c.width, 77);
        assert_eq!(c.height, 36); // no preset applied
    }

    #[test]
    fn default_values() {
        let c = Config::default();
        assert_eq!(c.width, 220);
        assert_eq!(c.height, 36);
        assert_eq!(c.fps, 60);
        assert!(c.font_path.is_none());
        assert!(c.background.is_none());
    }

    #[test]
    fn corner_radius_parses_and_defaults_none() {
        let c: Config = serde_json::from_str(r#"{ "corner_radius": 0.0 }"#).unwrap();
        assert_eq!(c.corner_radius, Some(0.0));
        assert!(Config::default().corner_radius.is_none());
    }

    #[test]
    fn stream_flag_parses_and_defaults_false() {
        let c: Config = serde_json::from_str(r#"{ "exec": "x", "stream": true }"#).unwrap();
        assert!(c.stream);
        // Absent -> false (poll mode), the conservative default.
        let d: Config = serde_json::from_str(r#"{ "exec": "x" }"#).unwrap();
        assert!(!d.stream);
    }

    #[test]
    fn drag_keys_parse_and_default_none() {
        let c: Config =
            serde_json::from_str(r#"{ "drag_id": "3", "on_drop": "mv {src} {dst}" }"#).unwrap();
        assert_eq!(c.drag_id.as_deref(), Some("3"));
        assert_eq!(c.on_drop.as_deref(), Some("mv {src} {dst}"));
        assert!(Config::default().drag_id.is_none());
        assert!(Config::default().on_drop.is_none());
    }

    #[test]
    fn drag_id_accepts_the_number_waybar_turns_it_into() {
        let c: Config = serde_json::from_str(r#"{ "drag_id": 3 }"#).unwrap();
        assert_eq!(c.drag_id.as_deref(), Some("3"));
    }

    #[test]
    fn partial_json_fills_defaults() {
        // Waybar only passes the keys the user set; the rest must default.
        let c: Config = serde_json::from_str(r#"{ "width": 400, "fps": 0 }"#).unwrap();
        assert_eq!(c.width, 400);
        assert_eq!(c.fps, 0);
        assert_eq!(c.height, 36); // untouched -> default
        assert!(c.font_path.is_none());
    }

    #[test]
    fn full_json_round_trips() {
        let c: Config = serde_json::from_str(
            r##"{ "width": 360, "height": 64, "fps": 30,
                  "font_path": "/x.ttf", "background": "#1e1e2e", "font_size": 18.0 }"##,
        )
        .unwrap();
        assert_eq!(c.width, 360);
        assert_eq!(c.font_path.as_deref(), Some("/x.ttf"));
        assert_eq!(c.background.as_deref(), Some("#1e1e2e"));
        assert_eq!(c.font_size, 18.0);
    }
}
