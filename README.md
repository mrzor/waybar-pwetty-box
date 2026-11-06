# pwetty-box

A [Waybar](https://github.com/Alexays/Waybar) **CFFI module** (Rust `cdylib`) that
draws elaborate, multiline text/icon **tiles** on the GPU.

Waybar loads the compiled `.so` in-process and hands the module a GTK widget. Each
tile is drawn as **two composited layers** onto a `GtkDrawingArea`:

- a **GPU layer** — [femtovg](https://github.com/femtovg/femtovg) rendered into an
  offscreen image on our own surfaceless EGL context (backgrounds, gradients, and
  future shader effects);
- a **text layer** — rich **Pango markup** drawn with PangoCairo on top.

Both go through **Cairo**, which gives true per-pixel transparency against a
translucent bar (see below) and lets **custom effect tags** (e.g. `<box>`) bridge
the two — positioned via the Pango layout, drawn by the GPU/Cairo layer.

## Interoperability constraints (why it's built this way)

Verified against the installed `waybar` binary (`v0.15.0`) and GTK `3.24.52`:

| Constraint | Consequence |
|---|---|
| Waybar links **GTK3** (`libgtk-3`, `libgtkmm-3.0`); the CFFI ABI's `get_root_widget()` returns a GTK3 `GtkContainer*` | We are a GTK3 in-process widget. (`waybar-cffi` binds gtk-rs 0.18.) |
| Waybar links **no Vulkan**, and GTK3 has no Vulkan surface widget | Rendering is OpenGL (via femtovg). We create our own **surfaceless EGL** context on the DRM render node — no window, no seat, no DRM-master. |
| **`GtkGLArea` cannot alpha-composite against a translucent bar** in GTK3 (verified hardware *and* software on 3.24.52: transparent regions render as opaque black) | We do **not** use `GtkGLArea`. Instead we render femtovg offscreen, read it back, and composite via **Cairo** onto a `GtkDrawingArea` — Cairo honors per-pixel alpha, so the tile is genuinely transparent against a see-through bar. Cost is a small GPU→CPU readback per frame, negligible for a bar tile. |

## Build

The crate's MSRV is **1.88** (`waybar-cffi` needs ≥1.85, `femtovg` ≥1.88). A
`rust-toolchain.toml` pins the toolchain to `1.92`.

```bash
cargo build --release
# -> target/release/libpwetty_box.so
```

## Use in Waybar

See [`examples/waybar-config.jsonc`](examples/waybar-config.jsonc). Minimal:

```jsonc
"modules-right": ["cffi/pwetty"],
"cffi/pwetty": {
  "module_path": "/abs/path/to/target/release/libpwetty_box.so",
  "width": 360, "height": 64, "fps": 60
}
```

Reload Waybar (`killall -SIGUSR2 waybar`, or restart). You should see an animated
gradient pill with a label and an icon glyph — the demo tile proving the pipeline.

### Config options

All keys live inside the `cffi/pwetty` block (parsed by `src/config.rs`):

| Key | Default | Meaning |
|---|---|---|
| `width` / `height` | `220` / `36` | Tile size in logical pixels. |
| `fps` | `60` | Animation framerate; `0` = static/content-driven (redraw only when content changes). |
| `text` | _(unset)_ | Static data for the template. Use for fixed content. |
| `exec` | _(unset)_ | Shell command; its stdout is the tile's **data** (JSON if parseable, else plain text). |
| `interval` | `0` | Re-run cadence for `exec`, in seconds (`0` = run once). Ignored when `stream` is set. |
| `stream` | `false` | **Push mode** for `exec`: spawn the command **once** and treat each newline-delimited stdout line as fresh data (sub-150ms repaint), instead of polling on `interval`. The command stays alive and prints one JSON object per line whenever data changes; on EOF/exit the last content is kept and the command respawns after a ~1s backoff. Use for event-driven sources (e.g. a niri desktop-switch watcher) that need to update faster than a poll. |
| `drag_id` | _(unset)_ | Makes the tile **draggable** onto any other tile of the same bar that also sets one (and a drop target for them); the id travels as the drag payload. |
| `on_drop` | _(unset)_ | Shell command run on the tile a drag lands **on**, with `{src}`/`{dst}` replaced by the dragged and the receiving tile's `drag_id` — e.g. `"claude-status tile-move {src} {dst}"` to reorder niri desktops by dragging their tiles. |
| `icon` | _(unset)_ | Glyph prepended to the content, sized + vertically centered on the text. |
| `format` | `"{{ value }}"` | **Template** ([minijinja](https://github.com/mitsuhiko/minijinja)) rendered against the data → a Pango-markup string. See below. |
| `font_size` | `14.0` | Base text size in pixels (per-span sizes via markup override it). |
| `background` | _(transparent)_ | Tile background as `#rrggbb` / `#rrggbbaa`. Leave unset for a **transparent** tile (the bar shows through); set it for an opaque background. |
| `background_shader` | _(unset)_ | Path to a Shadertoy-style GLSL fragment shader rendered as the tile's animated background (see below). Hot-reloaded on file change. |
| `shader_uniforms` | _(unset)_ | Map of `float` uniform → template (e.g. `{ "u_load": "{{ cpu.pct }}" }`), resolved from the data so the shader reacts to it. |
| `font_path` / `icon_font_path` | _(system)_ | Fonts for the **demo tile** (femtovg) only; content tiles render via Pango using system fonts. |

With no `text`/`exec`, the module renders the animated demo tile. With either set,
it renders a content tile; `exec` refreshes on a background thread, so a slow
command never blocks the bar.

### Data → template → tile

The model is **data-bound templates**. A command emits a **JSON object** (the
data); `format` is a [minijinja](https://github.com/mitsuhiko/minijinja) template
(Jinja-style `{{ … }}` / `{% … %}`) that binds fields into **Pango markup**:

```jsonc
"exec": "sysinfo.sh",   // prints e.g. {"host":"nas","cpu":{"pct":82,"color":"#fab387"},"mem":{"used":"7.1G"}}
"interval": 2,
"format": "<span size='xx-large' weight='bold'>{{ host }}</span>\n<span foreground='{{ cpu.color }}'>CPU {{ cpu.pct }}%</span>  MEM {{ mem.used }}\n{% if cpu.pct >= 90 %}<span foreground='#f38ba8' weight='bold'>⚠ high</span>{% endif %}"
```

- **Binding:** `{{ host }}`, `{{ cpu.pct }}`, `{{ items[0].name }}` — object fields are top-level; a non-object (plain-text) command is available as `{{ value }}`.
- **Safety:** bound values are auto-escaped (XML), so command output can't break the markup; the template's own `<span>`s are preserved.
- **Logic:** filters (`{{ x | round }}`, `{{ y | default('?') }}`) and `{% if %}`/`{% for %}` — so **state styling lives in the data or the template** (the script picks a colour, *or* the template branches on a threshold). No separate "states" system.

On top of standard Pango tags, **custom effect tags** are extracted and drawn by
our own renderer, positioned via the Pango layout:

```jsonc
"format": "vol <box bg='#f38ba8cc'>{{ value }}</box>"   // rounded highlight behind the value
```

Implemented effect tags:
- `<box bg='#rrggbb[aa]'>…</box>` — a Cairo rounded highlight behind the span.
- `<glow color='#rrggbb'>…</glow>` — a soft, gently pulsing **GPU-shader** halo
  behind the span (a built-in shader rendered through the shared shader cache).

Both are positioned via the Pango layout (`markup::process` → effect span →
`text::span_rect` → draw). A user-supplied per-span `<shader src='…'>` is the next
addition on the same seam. Combine with `{% if %}` for conditional effects (e.g.
glow a value only when it's critical).

### Inline embeds & the ticker

Some elements are **placed inline** in the text flow rather than laid out as
glyphs — they reserve a fixed-width box that surrounding text composes around,
**across multiple lines** (`\n` in the template starts a new line). The embeds
that ship today:

- `<wrap>…</wrap>` — a **word-wrapped text block**: the inner text flows onto as
  many lines as it needs (within the tile width), vertically centered as a block.
  Static — no motion. This is how the bundled `claude` tile renders titles.
- `<tickerbox width="N">…</tickerbox>` — a **scrolling marquee** for content
  wider than its box: clipped, scrolling briskly, looping with a `◆` seam marker.
  Renders static (no scroll) when the content fits. (Animated — prefer `<wrap>`
  for long static text; reach for the marquee only when horizontal motion is the
  point, since it forces a per-frame repaint.)
- `<status state="…" level="N"/>` — an **animated session indicator**: `working`
  and `shell` render the pixel-art **Claude mascot** (orange / electric-cyan,
  each with a color-matched glow), `prompt` a blinking `?`, `idle` a static fade
  bar keyed by `level`. See the bundled `claude` tile below.
- `<icon name="…"/>` / `<icon src="…"/>` — an **SVG icon** (see below).
- `<sep/>` — a thin gap that splits adjacent text into independent runs (so a
  digit and a word don't share one Pango baseline).

Inline symbols (status, icons) are sized to the **ink box of the digit/text
beside them** and centered on the line, so they read as peers — no font-glyph
baseline wrangling.

```jsonc
"exec": "now-playing.sh",
"format": "<b>NOW</b>  <tickerbox width='300'>♪ {{ artist }} — {{ title }}</tickerbox>  {{ time }}"
```

→ a bold label, a 300px scrolling ticker, and a value — composed on one line.
Animation auto-enables; inner Pango markup is preserved. This inline-embed seam
(`markup::Embed` → measured placement → draw into the box, laid out by
`draw_flow`) is where future `<bar>`/`<ring>`/`<sparkline>` elements will live.
(v1: a tile uses either inline embeds or `<box>`/`<glow>` span effects, not both.)

### Icons (bundled SVGs)

Icons are **SVG**, not font glyphs — rasterized with `resvg` (pure Rust) at the
exact device resolution and composited onto the tile. That means crisp at any
size/scale, and *unlimited* — no font can give you every app's logo.

```jsonc
"format": "<icon name='git' color='#a6e3a1'/> {{ branch }}"
```

- `<icon name="…"/>` — a **bundled** icon (ships inside the module).
- `<icon src="/path/to.svg"/>` — any **external** SVG (e.g. a freedesktop app icon).
- `color="#rrggbb"` (optional) — tint the SVG as a monochrome silhouette; omit it
  to keep the artwork's own colours (e.g. a multi-colour app logo).
- `hero="1"` — draw the icon large in a left gutter, with the text indented past it.
- `watermark="1"` — draw the icon large and dimmed in the *background*, biased
  right; the text rides over it at full width, under a soft dark halo that keeps
  light text legible over the artwork. Used by the plain-window layout of the
  `claude` tile.

Each icon is sized to the neighbouring digit/text and centered on the line
(except `hero`/`watermark`, which scale to the tile).

**Bundled set:**

![bundled icons](docs/icons.png)

`folder` · `check` · `arrow-up` · `bell` · `code` · `terminal` · `gear` · `app`

(Add more by dropping `.svg` files in `icons/` and registering them in
`src/svg.rs`, or just point `src=` at your own files.)

### Bundled tiles & the data contract

Whole tiles can ship **inside the module**: geometry, fonts, colours, and the
`format` template, packaged as a named preset. A waybar module references one by
name and supplies only the data — pwetty layers the preset underneath, and the
module's own keys win:

```jsonc
"cffi/pwetty#claude5": {
  "module_path": ".../libpwetty_box.so",
  "tile": "claude",                 // bundled preset (geometry + template)
  "interval": 2,
  "exec": "claude-tile-data 5"      // your job: emit JSON matching the contract
}
```

Override any preset field inline (`"width": 360`), or point at an external file
to iterate without rebuilding (`"tile_file": "/path/tile.json"`).

These tiles are really niri **desktop** tiles. Two ship:

- **`claude`** — a desktop running a Claude session: shortcut number, an animated
  session-status indicator (`working`→deep-orange Claude mascot blink, `shell`→
  electric-cyan mascot pulse, both with a color-matched glow; `prompt`→pulsing `?`;
  `idle`→a fade bar by `idle_level`), the folder, an `↑N` unpushed-commits badge
  (or `idle_ago` when idle), and the **word-wrapped** window title on line 2. On
  `prompt` the whole tile pulses to pull your eye. When the desktop holds an
  ordinary window instead (`is_claude=false`), it shows the app icon + name. The
  **focused** desktop (`active=true`) gets an accent card.
- **`empty`** — a compact, narrow tile for a windowless desktop: just the shortcut
  number stacked over a dim "empty" ring, center-aligned. (`tile: "empty"`.)

The data source is **decoupled**: a tile declares the JSON it wants via a
JSON Schema, and the `pwetty` CLI surfaces that contract so a separate
data-gathering layer knows exactly what to emit:

```bash
pwetty list                  # bundled tiles + their samples
pwetty schema claude         # the tile's JSON Schema (the data contract)
pwetty check claude          # validate template ↔ schema ↔ bundled samples
pwetty render claude --all-states -o /tmp/out   # PNG per sample (needs surfaceless EGL)
echo '{…your json…}' | pwetty render claude --data -   # render YOUR data, eyeball it
```

The data layer's loop: read the schema, emit matching JSON, pipe it through
`render --data` to confirm it looks right, then wire it into the module's `exec`.

See `tiles/claude/` for the preset, schema, mocked samples, and a fuller
binding-contract doc.

### Background shaders (GPU)

`background_shader` points at a **Shadertoy-style GLSL** fragment shader that
fills the whole tile, behind the content:

```jsonc
"background_shader": "/path/to/aurora.glsl",
"fps": 30,   // animate
"format": "<span weight='bold' foreground='#ffffff'>{{ time }}</span>"
```

The shader defines `void mainImage(out vec4 fragColor, in vec2 fragCoord)` and
receives `iResolution` / `iTime` / `iFrame` (paste-from-shadertoy.com friendly).
It's rendered on our own GL context into a texture, read back, and composited as
the background; the Pango content draws on top. The file is **hot-reloaded** when
it changes, and compile errors are logged.

**Data-reactive shaders.** `shader_uniforms` binds tile data into `float`
uniforms the shader can use — so the background *responds* to the data:

```jsonc
"exec": "cpu-load.sh",                       // emits e.g. {"load": 6.4}
"background_shader": "reactive.glsl",         // declares: uniform float u_load;
"shader_uniforms": { "u_load": "{{ (load | float) / 8.0 }}" },
"format": "<span weight='bold' foreground='#ffffff'>load {{ load }}</span>"
```

Each uniform value is a template evaluated against the data (`true`/`false` → 1/0,
otherwise parsed as a float). See `examples/shaders/reactive.glsl` (calm teal →
intense red as `u_load` rises).

**Mild masked backgrounds — `<bg preset="…"/>`.** Where `background_shader` is a
full-bleed, opaque background, the `<bg>` markup tag is the *subtle* counterpart:
a **bundled preset** drawn as a faint translucent layer, **clipped to the focus
bubble** (the same rounded-rect as the active card) with a **sharp 20px edge
fade** — a graphical accent for *some* tiles, not a show-off.

```jsonc
"format": "<bg preset='night'/><span size='xx-large' weight='bold'>{{ shortcut }}</span> …"
```

- `preset` — a bundled shader (`night` = deep-blue sky + drifting nebula +
  twinkling stars; `caustic` = night-blue water). Registered in `src/shader.rs`
  (`shaders/*.glsl`).
- `alpha` — layer opacity (default `0.28`); `fade` — steep edge-cliff width in px
  (default `20`); `falloff` — the slow vignette radius in px (default = the
  bubble's short half-extent) stacked under the cliff, so the layer is brightest
  deep inside and gently fades toward the edges.
- Any other attribute becomes a uniform: a plain number → a `float` (e.g.
  `speed='0.4'`); a hex colour → three `name_r/g/b` floats.

`night` specifics: `alpha` controls only the **blue field** (gradient + nebula);
the **stars carry their own opacity** (`stars_alpha`, default `0.9`) so they stay
crisp while the field is faint. `stars="#rrggbb"` tints the stars (clamped,
defaults to cool blue-white). `stars_gain` (default `1`) lifts star brightness +
persistence: crank it with `stars_alpha="1"` and a warm `stars` colour to turn
the calm field into an actual **attention grab** — e.g.
`<bg preset='night' stars='#ffb84d' stars_alpha='1' stars_gain='2.6'/>`, pairing
nicely with `<pulse>` on a tile that wants your eye.

The mask mirrors the focus bubble exactly (one shared `focus_bubble()`), so it
lines up with the active-card border. Like any shader it repaints per frame
(capped at the anim fps), so reach for it on a *few* tiles, not all of them.

## Architecture / where to extend

```
src/
  lib.rs        CFFI Module impl — adds a GtkDrawingArea to Waybar's container.
                Its `draw` callback composes two layers: femtovg GPU layer +
                Pango text layer (`draw_content`), with `<box>` effects between.
  offscreen.rs  OffscreenGl: a self-owned surfaceless EGL context (render node;
                no window/seat/DRM-master) for running femtovg headless.
  gl.rs         Points the `epoxy` crate at the in-process libepoxy.
  render.rs     femtovg Canvas lifecycle, `capture()` (render to an offscreen
                image + read back RGBA), `parse_hex_color`.
  config.rs     serde Config deserialized from the `cffi/...` block.
  content.rs    ContentStore (thread-safe) + sources: a command's output is
                parsed as JSON data, bound through the template → a markup string.
  markup.rs     `render_template` (minijinja: data + template → markup) +
                >>> EFFECT SEAM <<< `process` (XML routing: Pango-safe markup +
                custom-tag EffectSpans) + escaping + `icon_span`. (Heavily tested.)
  text.rs       Pango/Cairo: lay out + paint markup; `span_rect` locates a span.
  shader.rs     ShaderPass (compile a Shadertoy-style GLSL shader, render to a
                texture, read back RGBA) + ShaderCache (compile-once by key) +
                the built-in <glow> shader. Used for tile + span shaders.
  tile.rs       femtovg `Tile` trait + `TileContext` + the animated `DemoTile`
                (shown when no content source is configured).
```

Render flow per frame: `DrawingArea::draw` → (1) only if the tile needs GL, make
the one shared, lazily-created EGL context current and render the femtovg/shader
background → premultiply → Cairo paint; (2) `draw_content_cached`: on a content
change, `markup::process` + Pango-lay-out the tile once into a cached static
layer (text with its halo, icons), cut at each moving embed; every frame then
blits that and redraws only the moving parts (status blink/glow, scrolling
ticker), in their original stacking order. Tiles the cache can't hold (`<bg>`,
plain effect tiles) take the direct `draw_content` path.

Redraws come from the frame clock and/or a content dirty-flag poll. Animated
tiles redraw at 45 fps by default (`fps` overrides it, and makes the tile
redraw every frame even when static); an animation tick repaints only the
moving parts' area unless the whole tile pulses. Measure changes with
`test/bench.sh` (a nested waybar with ~40 tiles) and
`examples/bench_frames.rs` (offscreen).

**To add a custom effect** (e.g. `<glow>`, `<shader>`): add the tag name to
`EFFECT_TAGS` in `lib.rs`, then handle it in `draw_content` — `text::span_rect`
gives you the pixel rect of its text, into which you draw (Cairo, or a femtovg
shader pass composited like the background layer).

## Testing & screenshots

- **Unit tests** (`cargo test`): the markup router + `render_template` binding
  (`markup.rs`), content (`build_markup`/`parse_data`), config, `parse_hex_color`
  — all pure logic, no GL/GTK context needed.
- **Vision tests** (offscreen → PNG, pure CPU, safe anywhere):
  ```bash
  # data → template → tile (JSON data bound into a multi-line template)
  cargo run --example render_data -- out.png            # default nas dashboard
  # a tile background shader, one frame (surfaceless GL — force software to be safe)
  EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
    cargo run --example render_shader -- out.png examples/shaders/aurora.glsl [time]
  # rich text via Pango/Cairo
  cargo run --example render_text -- out.png
  # content path (markup + <box>/<glow> effects, optional icon arg) via draw_content
  cargo run --example render_content -- out.png "CPU <glow color='#f38ba8'>96%</glow>" 44
  # the femtovg demo tile (surfaceless GL — force software so it can't touch your display)
  EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
    cargo run --example render_tile -- out.png [seconds]
  ```
  Inspect the PNGs by eye — these caught a transparency bug no unit test could.
- **Live waybar** (`test/`): `cage` (headless) → `niri` (nested) → `waybar` →
  `grim`, driven by `test/shot.sh`. ⚠️ Runs a nested compositor stack; read the
  safety notes in `test/shot.sh` (prefer a separate TTY).

## Notes

- femtovg fills paths through the **stencil buffer**; its offscreen image targets
  attach one automatically, so no GTK GL-area stencil setup is needed.
- Credit/inspiration: [waybar_shader_widget](https://codeberg.org/Frieder_Hannenheim/waybar_shader_widget)
  (a pure-GLSL Shadertoy-style sibling using `GtkGLArea` + the `gl` crate — it
  renders opaque full-bleed shaders, so it never hit the transparency limitation).
