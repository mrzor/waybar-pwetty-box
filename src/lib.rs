//! pwetty-box — a Waybar CFFI module that renders elaborate multiline text/icon
//! tiles on the GPU.
//!
//! Pipeline: Waybar (GTK3) hands us a `GtkContainer` via the CFFI ABI. We add a
//! [`DrawingArea`](gtk::DrawingArea) and render the tiles with femtovg into an
//! offscreen image on our own surfaceless EGL context, then composite that image
//! onto the widget with Cairo.
//!
//! Why not a `GtkGLArea` (the obvious choice)? It cannot alpha-composite its GL
//! contents against a translucent bar in GTK3 — verified on GTK 3.24.52, both
//! hardware and software: transparent regions render as opaque black. Cairo
//! honors per-pixel alpha, so the offscreen-render + Cairo-composite path gives
//! true transparency against a see-through waybar. Waybar is GTK3 and exposes no
//! Vulkan surface, so OpenGL (via femtovg) remains the rendering API.

pub mod config;
pub mod content;
pub mod gl;
pub mod markup;
pub mod offscreen;
pub mod render;
pub mod shader;
pub mod svg;
pub mod text;
pub mod tile;
pub mod tiles;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use waybar_cffi::gtk::{self, pango, prelude::*};
use waybar_cffi::{waybar_module, InitInfo, Module};

use config::Config;
use offscreen::OffscreenGl;
use render::Renderer;

thread_local! {
    /// The one offscreen EGL context every module on this (GTK main) thread
    /// renders with, created on first use — so a bar of pure-Cairo tiles never
    /// opens EGL at all, and N GL tiles share one context instead of N. Its GL
    /// objects (femtovg canvases, shader programs) are per-tile but live here.
    static SHARED_GL: Option<OffscreenGl> = OffscreenGl::new()
        .map_err(|e| eprintln!("pwetty-box: offscreen GL init failed: {e:?}"))
        .ok();
}

/// Make the shared GL context current (creating it on first call). False if
/// EGL is unavailable.
fn shared_gl_current() -> bool {
    SHARED_GL.with(|gl| gl.as_ref().is_some_and(|gl| gl.make_current().is_ok()))
}

/// A tile's rendering state. GL-side pieces are created lazily, in the shared
/// context, the first time a frame needs them.
struct Engine {
    /// femtovg canvas for the demo tile (no content source); `None` until used.
    renderer: Option<Renderer>,
    start: Instant,
    /// Optional tile-level background shader (path + lazily compiled + mtime).
    shader_path: Option<String>,
    shader: Option<shader::ShaderPass>,
    shader_mtime: Option<SystemTime>,
    /// Compiled shaders for span-level effects (e.g. `<glow>`), keyed by source.
    span_shaders: shader::ShaderCache,
    frame: i32,
}

impl Engine {
    /// Compile the background shader (or recompile if the file changed). A GL
    /// context must be current. Compile errors are logged and leave no shader.
    fn refresh_shader(&mut self) {
        let Some(path) = self.shader_path.clone() else {
            return;
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if self.shader.is_some() && mtime == self.shader_mtime {
            return; // unchanged
        }
        self.shader_mtime = mtime;
        match std::fs::read_to_string(&path) {
            Ok(src) => match shader::ShaderPass::new(&src) {
                Ok(p) => self.shader = Some(p),
                Err(e) => {
                    eprintln!("pwetty-box: shader compile error in '{path}':\n{e}");
                    self.shader = None;
                }
            },
            Err(e) => {
                eprintln!("pwetty-box: cannot read shader '{path}': {e}");
                self.shader = None;
            }
        }
    }
}

/// Shared between `init` and the draw/tick callbacks. GTK is single-threaded, so
/// `Rc`/`RefCell` suffices.
struct Shared {
    engine: RefCell<Engine>,
    config: Config,
    /// The tile's static layer, rebuilt when its content changes.
    layer: RefCell<Option<StaticLayer>>,
}

pub struct PwettyBox {
    // Keep shared state alive for the module's lifetime; the GTK widget tree
    // (owned by Waybar) holds the closures that reference it.
    _shared: Rc<Shared>,
}

/// Hover highlight: fade duration (ms), ring colour (light grey with a slight
/// blue cast), and peak stroke alpha.
const HOVER_FADE_MS: f64 = 160.0;
const HOVER_COLOR: &str = "#b9c1d9";
const HOVER_ALPHA: f64 = 0.6;
/// Peak content zoom on hover — the tile's text (a desktop's shortcut number,
/// say) swells a touch and settles back on leave. Discreet on purpose: past
/// ~1.08 a wide tile's content starts hitting the widget's clip edge.
const HOVER_SCALE: f64 = 1.06;

/// Pointer-hover fade state. A flip records the alpha it started from, so a
/// mid-fade reversal animates smoothly instead of jumping.
struct Hover {
    inside: Cell<bool>,
    flipped: Cell<Instant>,
    from_alpha: Cell<f64>,
}

impl Hover {
    fn new() -> Self {
        Self {
            inside: Cell::new(false),
            flipped: Cell::new(Instant::now()),
            from_alpha: Cell::new(0.0),
        }
    }

    /// Flip the hover state (no-op if unchanged), restarting the fade from the
    /// current alpha.
    fn set(&self, inside: bool) {
        if self.inside.get() == inside {
            return;
        }
        self.from_alpha.set(self.alpha());
        self.inside.set(inside);
        self.flipped.set(Instant::now());
    }

    /// Current ring alpha in `[0, 1]`, interpolating toward the state's target.
    fn alpha(&self) -> f64 {
        let p = (self.flipped.get().elapsed().as_secs_f64() * 1000.0 / HOVER_FADE_MS).min(1.0);
        let target = if self.inside.get() { 1.0 } else { 0.0 };
        self.from_alpha.get() + (target - self.from_alpha.get()) * p
    }

    /// Content zoom factor, tracking the same fade as [`Hover::alpha`] so the
    /// ring and the scale move together (and reverse together mid-fade).
    fn scale(&self) -> f64 {
        1.0 + self.alpha() * (HOVER_SCALE - 1.0)
    }

    /// True while a fade is still in progress.
    fn animating(&self) -> bool {
        self.flipped.get().elapsed().as_secs_f64() * 1000.0 < HOVER_FADE_MS
    }
}

/// Redraw `area` at ~30fps until `hover`'s fade completes. Each flip spawns one;
/// a stale timer self-terminates after one extra tick.
fn animate_hover(area: &gtk::DrawingArea, hover: &Rc<Hover>) {
    let area = area.clone();
    let hover = hover.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(33), move || {
        area.queue_draw();
        if hover.animating() {
            gtk::glib::ControlFlow::Continue
        } else {
            gtk::glib::ControlFlow::Break
        }
    });
}

impl Module for PwettyBox {
    // Take the raw JSON so we can layer a bundled tile preset (`"tile": "..."`)
    // under it before deserializing into the typed `Config`.
    type Config = serde_json::Value;

    fn init(info: &InitInfo, raw: serde_json::Value) -> Self {
        let config = config::resolve(raw);
        let container = info.get_root_widget();

        let area = gtk::DrawingArea::new();
        area.set_size_request(config.width, config.height);
        area.set_hexpand(false);
        area.set_vexpand(false);

        // Pointer-hover highlight: a thin grey-blue ring fading in/out (see
        // `Hover`). Enter/leave must be in the event mask before realize.
        area.add_events(
            gtk::gdk::EventMask::ENTER_NOTIFY_MASK | gtk::gdk::EventMask::LEAVE_NOTIFY_MASK,
        );
        let hover = Rc::new(Hover::new());
        {
            let hover = hover.clone();
            area.connect_enter_notify_event(move |area, _| {
                hover.set(true);
                animate_hover(area, &hover);
                glib_propagation_proceed()
            });
        }
        {
            let hover = hover.clone();
            area.connect_leave_notify_event(move |area, _| {
                hover.set(false);
                animate_hover(area, &hover);
                glib_propagation_proceed()
            });
        }

        // Content source (static text / refreshing command), if configured.
        let content = content::from_config(&config);

        // No GL work at init: the shared context and this tile's GL objects
        // come up on the first frame that needs them.
        let engine = Engine {
            renderer: None,
            start: Instant::now(),
            shader_path: config.background_shader.clone(),
            shader: None,
            shader_mtime: None,
            span_shaders: shader::ShaderCache::new(),
            frame: 0,
        };

        let shared = Rc::new(Shared {
            engine: RefCell::new(engine),
            config,
            layer: RefCell::new(None),
        });

        {
            let shared = shared.clone();
            let content_draw = content.clone();
            let hover = hover.clone();
            area.connect_draw(move |area, cr| {
                let scale = area.scale_factor().max(1);
                // Shader uniforms resolved from the current data (empty if none).
                let shader_uniforms = content_draw
                    .as_ref()
                    .map(|s| s.uniforms())
                    .unwrap_or_default();

                {
                    let engine = &mut *shared.engine.borrow_mut();
                    let wd = (area.allocated_width().max(1) * scale) as u32;
                    let hd = (area.allocated_height().max(1) * scale) as u32;
                    let wl = area.allocated_width().max(1) as f64;
                    let hl = area.allocated_height().max(1) as f64;
                    let time = engine.start.elapsed().as_secs_f32();

                    // Current content markup (content tiles only).
                    let markup = content_draw.as_ref().map(|s| s.markup());
                    let has_content = markup.is_some();

                    // GL is only needed for a background shader, the demo tile
                    // (femtovg), or a span `<glow>`. Pure-Cairo content tiles
                    // (dots/icons/ticker/pulse are all CPU) skip the EGL
                    // `make_current` entirely — a real per-frame saving that lets
                    // us animate at a higher rate without melting the laptop.
                    let needs_glow = markup.as_deref().is_some_and(|m| m.contains("<glow"));
                    let needs_bg = markup.as_deref().is_some_and(|m| m.contains("<bg"));
                    let needs_gl =
                        !has_content || engine.shader_path.is_some() || needs_glow || needs_bg;

                    if !needs_gl || shared_gl_current() {
                        engine.refresh_shader();
                        let frame = engine.frame;

                        // Layer 1: background. A shader renders each frame; the demo
                        // tile uses femtovg; a content tile's background is static, so
                        // just fill it in Cairo (no per-frame GPU render+readback).
                        let bg: Option<Vec<u8>> = if let Some(sh) = engine.shader.as_mut() {
                            Some(sh.render(wd as i32, hd as i32, time, frame, &shader_uniforms))
                        } else if !has_content && engine.shader_path.is_none() {
                            // Demo tile: femtovg canvas, built on first use.
                            if engine.renderer.is_none() {
                                engine.renderer = Renderer::new(&shared.config, false)
                                    .map_err(|e| {
                                        eprintln!("pwetty-box: renderer init failed: {e:?}")
                                    })
                                    .ok();
                            }
                            engine
                                .renderer
                                .as_mut()
                                .and_then(|r| r.capture(wd, hd, scale as f32, time).ok())
                                .map(|(_, _, rgba)| rgba)
                        } else {
                            None
                        };
                        if engine.shader.is_some() {
                            engine.frame = engine.frame.wrapping_add(1);
                        }
                        if let Some(rgba) = bg {
                            composite_rgba(cr, wd as usize, hd as usize, rgba, scale as f64);
                        } else if has_content && engine.shader_path.is_none() {
                            if let Some(c) = shared
                                .config
                                .background
                                .as_deref()
                                .and_then(render::parse_hex_color)
                            {
                                cr.set_source_rgba(c.r as f64, c.g as f64, c.b as f64, c.a as f64);
                                let _ = cr.paint();
                            }
                        }

                        // Layer 2: Pango text + inline embeds / span effects.
                        if let Some(markup) = &markup {
                            let mut fx = EffectCtx {
                                shaders: &mut engine.span_shaders,
                                time,
                                frame,
                                scale: scale as f64,
                            };
                            // Hover zoom: swell the content about the tile centre
                            // (background/ring stay put) so the number grows in
                            // place and settles back on leave. The hover timer
                            // queues whole-widget redraws while it fades, so the
                            // static layer's damage rects are never trusted mid-zoom.
                            let hs = hover.scale();
                            let _ = cr.save();
                            if hs > 1.0 {
                                cr.translate(wl / 2.0, hl / 2.0);
                                cr.scale(hs, hs);
                                cr.translate(-wl / 2.0, -hl / 2.0);
                            }
                            draw_content_cached(
                                cr,
                                markup,
                                wl,
                                hl,
                                &shared.config,
                                Some(&mut fx),
                                &mut shared.layer.borrow_mut(),
                            );
                            let _ = cr.restore();
                        }
                    }
                }

                // Hover highlight: a thin ring on the focus bubble, fading
                // in/out with pointer hover. Drawn last, over the content.
                let a = hover.alpha();
                if a > 0.01 {
                    let wl = area.allocated_width().max(1) as f64;
                    let hl = area.allocated_height().max(1) as f64;
                    let (x, y, pw, ph, r) = focus_bubble(wl, hl);
                    rounded_rect(cr, x, y, pw, ph, r);
                    set_hex(cr, HOVER_COLOR, a * HOVER_ALPHA);
                    cr.set_line_width(1.0);
                    let _ = cr.stroke();
                }

                glib_propagation_proceed()
            });
        }

        // Animate by redrawing on the frame clock — but ONLY while there's
        // actually something moving. The frame clock fires at the monitor's rate
        // (~60Hz); we throttle queue_draw to a target fps, AND each tick we check
        // whether the *current* content animates (a blinking status / `<pulse>` /
        // marquee). A static tile (idle/empty/plain wrapped text) queues nothing —
        // it only repaints when its data changes (the dirty poll below). This is
        // what keeps a bar full of mostly-idle desktops cool.
        let forced = shared.config.fps > 0 || shared.config.background_shader.is_some();
        let could_anim = forced
            || shared.config.format.as_deref().is_some_and(|f| {
                f.contains("<tickerbox")
                    || f.contains("<status")
                    || f.contains("<pulse")
                    || f.contains("<bg")
            });
        if could_anim {
            let target_fps = if shared.config.fps > 0 {
                shared.config.fps
            } else {
                DEFAULT_ANIM_FPS
            };
            let min_dt = 1_000_000 / target_fps.max(1) as i64; // microseconds
                                                               // 0 sentinel (not i64::MIN — `now - i64::MIN` overflows): the first
                                                               // tick's `now - 0` is huge, so it draws immediately, then throttles.
            let last = std::cell::Cell::new(0i64);
            let tick_content = content.clone();
            let tick_shared = shared.clone();
            area.add_tick_callback(move |area, clock| {
                let animating = forced || tick_content.as_ref().is_some_and(|s| s.animating());
                if animating {
                    let now = clock.frame_time();
                    if now - last.get() >= min_dt {
                        last.set(now);
                        queue_frame(area, forced, &tick_shared.layer.borrow());
                    }
                }
                gtk::glib::ControlFlow::Continue
            });
        }

        // Redraw when a content source publishes new content (e.g. a command
        // refresh). Cheap poll of the dirty flag — content tiles can set fps: 0.
        if let Some(store) = content {
            let area = area.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(150), move || {
                if store.take_dirty() {
                    area.queue_draw();
                }
                gtk::glib::ControlFlow::Continue
            });
        }

        container.add(&area);
        area.show();

        PwettyBox { _shared: shared }
    }
}

/// Queue an animation frame: only the cached layer's live regions when that's
/// all that moves (a status blink is a small fraction of the tile), else the
/// whole widget. `forced` tiles (fps/shader) repaint everything every frame.
fn queue_frame(area: &gtk::DrawingArea, forced: bool, layer: &Option<StaticLayer>) {
    let damage = if forced {
        None
    } else {
        layer.as_ref().and_then(StaticLayer::frame_damage)
    };
    let Some(rects) = damage else {
        area.queue_draw();
        return;
    };
    for (x, y, w, h) in rects {
        // Round outward to whole logical px so no inked edge is left stale.
        let (x0, y0) = (x.floor() as i32, y.floor() as i32);
        let (x1, y1) = ((x + w).ceil() as i32, (y + h).ceil() as i32);
        area.queue_draw_area(x0, y0, x1 - x0, y1 - y0);
    }
}

/// Render `markup` for `config`'s tile size to an RGBA PNG at `out`, via the
/// exact offscreen compose path the live widget uses (femtovg background layer +
/// Cairo [`draw_content`]) on a translucent dark backing (so transparent tiles
/// stay visible). Pure CPU + surfaceless EGL, so it's safe anywhere — used by
/// the `pwetty` CLI and the vision harness. Run with `EGL_PLATFORM=surfaceless
/// LIBGL_ALWAYS_SOFTWARE=1` for a headless software render.
pub fn render_png(
    config: &Config,
    markup: &str,
    time: f32,
    out: &std::path::Path,
) -> Result<(), String> {
    use gtk::cairo::{Context, Format, ImageSurface};

    let w = config.width.max(1);
    let h = config.height.max(1);
    let surface = ImageSurface::create(Format::ARgb32, w, h).map_err(|e| e.to_string())?;
    let cr = Context::new(&surface).map_err(|e| e.to_string())?;

    // Translucent dark backing, like a bar, so transparent tiles are visible.
    cr.set_source_rgba(
        0x1e as f64 / 255.0,
        0x1e as f64 / 255.0,
        0x2e as f64 / 255.0,
        0.85,
    );
    let _ = cr.paint();

    let gl = OffscreenGl::new().map_err(|e| format!("offscreen EGL: {e:?}"))?;
    gl.make_current()
        .map_err(|e| format!("make_current: {e:?}"))?;

    // Faithful to the live content path: femtovg background first (it dirties GL
    // state), then the Pango content + span effects on top.
    if let Ok(mut renderer) = Renderer::new(config, true) {
        if let Ok((rw, rh, rgba)) = renderer.capture(w as u32, h as u32, 1.0, time) {
            composite_rgba(&cr, rw, rh, rgba, 1.0);
        }
    }

    let mut cache = shader::ShaderCache::new();
    let mut fx = EffectCtx {
        shaders: &mut cache,
        time,
        frame: 0,
        scale: 1.0,
    };
    draw_content(&cr, markup, w as f64, h as f64, config, Some(&mut fx));

    drop(cr);
    let mut f = std::fs::File::create(out).map_err(|e| e.to_string())?;
    surface.write_to_png(&mut f).map_err(|e| e.to_string())?;
    Ok(())
}

/// Composite an offscreen RGBA8 buffer (straight alpha, top-left origin) onto the
/// Cairo context, honoring per-pixel alpha and scaling device pixels back to the
/// logical area. Public so an offscreen harness can reproduce the live compose.
pub fn composite_rgba(
    cr: &gtk::cairo::Context,
    w: usize,
    h: usize,
    rgba: Vec<u8>,
    device_scale: f64,
) {
    paint_rgba_at(cr, w, h, rgba, device_scale, 0.0, 0.0);
}

/// As [`composite_rgba`], but places the image's top-left at logical `(ox, oy)` —
/// used to composite a span effect (e.g. a glow) at its position.
fn paint_rgba_at(
    cr: &gtk::cairo::Context,
    w: usize,
    h: usize,
    mut rgba: Vec<u8>,
    device_scale: f64,
    ox: f64,
    oy: f64,
) {
    use gtk::cairo::{Format, ImageSurface};

    // femtovg gives straight-alpha RGBA; Cairo ARGB32 wants premultiplied, in
    // native-endian byte order (little-endian: B, G, R, A).
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        let r = (px[0] as u32 * a / 255) as u8;
        let g = (px[1] as u32 * a / 255) as u8;
        let b = (px[2] as u32 * a / 255) as u8;
        px[0] = b;
        px[1] = g;
        px[2] = r;
        px[3] = a as u8;
    }

    let stride = 4 * w as i32;
    let surface =
        match ImageSurface::create_for_data(rgba, Format::ARgb32, w as i32, h as i32, stride) {
            Ok(s) => s,
            Err(_) => return,
        };

    // We rendered at device resolution; place at the logical offset and scale
    // back. save/restore so this transform doesn't leak into the text layer.
    let _ = cr.save();
    cr.translate(ox, oy);
    let s = 1.0 / device_scale;
    cr.scale(s, s);
    if cr.set_source_surface(&surface, 0.0, 0.0).is_ok() {
        let _ = cr.paint();
    }
    let _ = cr.restore();
}

/// Custom effect tags (decorations drawn behind a span).
const EFFECT_TAGS: &[&str] = &["box", "glow"];
/// Inline embed tags (sized elements placed in the text flow). `sep` is a thin
/// spacer that draws nothing — its purpose is to *split a text run*, so adjacent
/// differently-sized spans each get independently vertically-centered (a single
/// Pango run would baseline-align them instead).
const EMBED_TAGS: &[&str] = &["tickerbox", "status", "icon", "sep", "wrap", "gutter"];

/// Default redraw rate for auto-animated tiles (blink/pulse/ticker) when the
/// config doesn't pin `fps`. Below the monitor's 60Hz but smooth: an animated
/// frame only repaints its cached layer's moving parts, so the extra
/// smoothness is cheap. Override per-module with `fps`.
const DEFAULT_ANIM_FPS: u32 = 45;

/// GPU resources + timing a span effect needs (currently `<glow>`). Without it
/// (e.g. a CPU-only caller), GPU span effects are skipped; `<box>` still draws.
pub struct EffectCtx<'a> {
    pub shaders: &'a mut shader::ShaderCache,
    pub time: f32,
    pub frame: i32,
    pub scale: f64,
}

/// Draw the content's Pango markup onto `cr` within a `w`×`h` logical tile,
/// rendering custom effect tags (`<box>`, `<glow>`) behind the text.
/// Public so offscreen vision harnesses can exercise the exact compose path.
pub fn draw_content(
    cr: &gtk::cairo::Context,
    content_markup: &str,
    w: f64,
    h: f64,
    config: &Config,
    mut fx: Option<&mut EffectCtx>,
) {
    let time = fx.as_ref().map(|f| f.time).unwrap_or(0.0);
    let scale = fx.as_ref().map(|f| f.scale).unwrap_or(1.0);
    let processed = markup::process(content_markup, EFFECT_TAGS, EMBED_TAGS);

    let style = text::TextStyle {
        font_family: font_family(config),
        size_px: config.font_size as f64,
        color: (0.95, 0.95, 1.0, 1.0),
        align_center: false,
    };

    // `<bg preset="…">` paints a shader layer (masked to the focus bubble, mild
    // alpha) as the bottom-most layer — behind the accent card and the content.
    if let (Some(bg), Some(fx)) = (&processed.bg, fx.as_deref_mut()) {
        draw_bg(cr, w, h, bg, fx);
    }

    // `<active/>` marks the focused desktop — a steady accent "card" behind the
    // tile (drawn before the pulse group so it doesn't oscillate).
    if processed.active {
        draw_active_panel(cr, w, h);
    }

    // `<pulse>` makes the whole tile's opacity oscillate (an attention signal):
    // render the content into a group, then paint it at a time-varying alpha.
    if processed.pulse {
        cr.push_group();
    }

    // Tiles with inline embeds compose explicitly (line-by-line, left-to-right);
    // tiles without use the richer wrapping + effect path.
    if !processed.embeds.is_empty() {
        draw_flow(cr, &processed, w, h, config, &style, time, scale, None);
    } else {
        let (layout, ox, oy) = text::layout(cr, &processed.markup, w, h, &style);

        // Effects render behind the text.
        for effect in &processed.effects {
            let rect = text::span_rect(&layout, ox, oy, effect.start, effect.end);
            match effect.tag.as_str() {
                "box" => draw_box(cr, rect, &effect.attrs),
                "glow" => {
                    if let Some(fx) = fx.as_deref_mut() {
                        draw_glow(cr, rect, &effect.attrs, fx);
                    }
                }
                _ => {}
            }
        }

        text::paint(cr, &layout, ox, oy, &style);
    }

    if processed.pulse {
        let _ = cr.pop_group_to_source();
        // Bright floor (0.4): the tile dims but never disappears.
        let _ = cr.paint_with_alpha(osc(time, PULSE_PERIOD, 0.40));
    }
}

/// A time-driven flow element, positioned by layout but drawn per frame — what
/// a cached static layer can't hold.
pub enum LiveOp {
    Status {
        attrs: Vec<(String, String)>,
        cx: f64,
        cy: f64,
        cap_h: f64,
    },
    Ticker {
        inner: String,
        rect: text::Rect,
    },
}

impl LiveOp {
    /// Logical-px box `(x, y, w, h)` the op can ink, generously padded: the
    /// widest status draw is the idle glow (radius `1.9·cap_h`, centred left of
    /// `cx`) plus the badge's age label (to `cx + 1.5·cap_h`, then its text).
    /// Only this needs repainting when the op moves.
    pub fn bounds(&self) -> text::Rect {
        match self {
            LiveOp::Status { cx, cy, cap_h, .. } => {
                (cx - 3.0 * cap_h, cy - 2.2 * cap_h, 6.5 * cap_h, 4.4 * cap_h)
            }
            LiveOp::Ticker { rect, .. } => *rect,
        }
    }

    fn draw(&self, cr: &gtk::cairo::Context, config: &Config, family: &str, time: f32, scale: f64) {
        match self {
            LiveOp::Status {
                attrs,
                cx,
                cy,
                cap_h,
            } => draw_status(cr, attrs, *cx, *cy, *cap_h, family, time, scale),
            LiveOp::Ticker { inner, rect } => draw_ticker(cr, inner, *rect, config, time),
        }
    }
}

/// Records a flow tile's drawing as alternating static rasters and [`LiveOp`]s,
/// in draw order: each live op seals the raster drawn so far and starts a new
/// one, so a moving element still sits between what's under and over it.
pub struct Segments {
    size: (i32, i32),
    scale: f64,
    /// Sealed `(raster, live op drawn right after it)` pairs.
    done: Vec<(gtk::cairo::ImageSurface, LiveOp)>,
    current: gtk::cairo::ImageSurface,
}

impl Segments {
    fn new(size: (i32, i32), scale: f64) -> Option<(Self, gtk::cairo::Context)> {
        let current = Self::surface(size)?;
        let cr = Self::context(&current, scale)?;
        let segs = Self {
            size,
            scale,
            done: Vec::new(),
            current,
        };
        Some((segs, cr))
    }

    fn surface(size: (i32, i32)) -> Option<gtk::cairo::ImageSurface> {
        use gtk::cairo::{Format, ImageSurface};
        ImageSurface::create(Format::ARgb32, size.0.max(1), size.1.max(1)).ok()
    }

    fn context(s: &gtk::cairo::ImageSurface, scale: f64) -> Option<gtk::cairo::Context> {
        let cr = gtk::cairo::Context::new(s).ok()?;
        cr.scale(scale, scale);
        Some(cr)
    }

    /// Seal the raster drawn so far, queue `op` after it, and return the
    /// context for the next raster — or hand `op` back if one couldn't be
    /// allocated (drawing then continues on the current raster).
    fn cut(&mut self, op: LiveOp) -> Result<gtk::cairo::Context, LiveOp> {
        let Some((next, next_cr)) =
            Self::surface(self.size).and_then(|s| Self::context(&s, self.scale).map(|cr| (s, cr)))
        else {
            return Err(op);
        };
        let sealed = std::mem::replace(&mut self.current, next);
        sealed.flush();
        self.done.push((sealed, op));
        Ok(next_cr)
    }
}

/// A flow tile pre-rendered as [`Segments`], rebuilt only when its markup, size
/// or scale changes. Each frame blits the rasters and redraws the live ops
/// between them.
pub struct StaticLayer {
    key: (String, i32, i32, u32),
    segments: Vec<(gtk::cairo::ImageSurface, LiveOp)>,
    tail: gtk::cairo::ImageSurface,
    pulse: bool,
    active: bool,
}

impl StaticLayer {
    /// What an animation frame must repaint, in logical px: the live ops'
    /// boxes, or `None` for the whole tile (a `<pulse>` fades all of it).
    /// Everything else is unchanged between frames.
    pub fn frame_damage(&self) -> Option<Vec<text::Rect>> {
        if self.pulse {
            return None;
        }
        Some(self.segments.iter().map(|(_, op)| op.bounds()).collect())
    }
}

/// Whether [`draw_content_cached`] can serve `processed` from a static layer:
/// flow tiles (inline embeds) without a per-frame `<bg>` shader layer. Plain
/// effect tiles (`<glow>`) keep the direct path.
fn cacheable(processed: &markup::Processed) -> bool {
    !processed.embeds.is_empty() && processed.bg.is_none()
}

/// [`draw_content`], served from `cache` when possible: the static parts of the
/// tile are drawn once per content change, and each frame only blits them and
/// redraws the moving embeds (status blink/glow, ticker) in their original
/// order. Falls back to [`draw_content`] for tiles the cache can't hold.
pub fn draw_content_cached(
    cr: &gtk::cairo::Context,
    content_markup: &str,
    w: f64,
    h: f64,
    config: &Config,
    fx: Option<&mut EffectCtx>,
    cache: &mut Option<StaticLayer>,
) {
    let time = fx.as_ref().map(|f| f.time).unwrap_or(0.0);
    let scale = fx.as_ref().map(|f| f.scale).unwrap_or(1.0);
    let key = (
        content_markup.to_string(),
        (w * scale).round() as i32,
        (h * scale).round() as i32,
        (scale * 64.0).round() as u32,
    );

    if cache.as_ref().is_none_or(|c| c.key != key) {
        *cache = None;
        let processed = markup::process(content_markup, EFFECT_TAGS, EMBED_TAGS);
        if !cacheable(&processed) {
            draw_content(cr, content_markup, w, h, config, fx);
            return;
        }
        *cache = build_static_layer(&processed, key, w, h, config, scale);
    }
    let Some(layer) = cache.as_ref() else {
        draw_content(cr, content_markup, w, h, config, fx);
        return;
    };

    let family = font_family(config);
    let blit = |s: &gtk::cairo::ImageSurface| {
        let _ = cr.save();
        cr.scale(1.0 / scale, 1.0 / scale);
        if cr.set_source_surface(s, 0.0, 0.0).is_ok() {
            let _ = cr.paint();
        }
        let _ = cr.restore();
    };
    // The active card sits outside the pulse group (it mustn't oscillate), as
    // in `draw_content`; it's two cheap Cairo ops, so it isn't cached.
    if layer.active {
        draw_active_panel(cr, w, h);
    }
    if layer.pulse {
        cr.push_group();
    }
    for (raster, op) in &layer.segments {
        blit(raster);
        op.draw(cr, config, &family, time, scale);
    }
    blit(&layer.tail);
    if layer.pulse {
        let _ = cr.pop_group_to_source();
        let _ = cr.paint_with_alpha(osc(time, PULSE_PERIOD, 0.40));
    }
}

/// Pre-render a flow tile as [`Segments`] at device `scale`.
fn build_static_layer(
    processed: &markup::Processed,
    key: (String, i32, i32, u32),
    w: f64,
    h: f64,
    config: &Config,
    scale: f64,
) -> Option<StaticLayer> {
    let (mut segs, cr) = Segments::new((key.1, key.2), scale)?;
    let style = text::TextStyle {
        font_family: font_family(config),
        size_px: config.font_size as f64,
        color: (0.95, 0.95, 1.0, 1.0),
        align_center: false,
    };
    draw_flow(
        &cr,
        processed,
        w,
        h,
        config,
        &style,
        0.0,
        scale,
        Some(&mut segs),
    );
    drop(cr);
    segs.current.flush();
    Some(StaticLayer {
        key,
        segments: segs.done,
        tail: segs.current,
        pulse: processed.pulse,
        active: processed.active,
    })
}

/// Measured rendered ink extents `(top, height)` keyed by (run markup, family,
/// base size ×4) — so we don't re-rasterize-and-scan a run every frame.
type InkCache = HashMap<(String, String, u32), (f64, f64)>;

thread_local! {
    static INK_CACHE: RefCell<InkCache> = RefCell::new(HashMap::new());
}

/// True rendered ink `(top, height)` of a flow text run, cached. Falls back to a
/// reasonable cap box if measurement fails.
fn run_ink(seg: &str, family: &str, base_px: f64) -> (f64, f64) {
    let key = (
        seg.to_string(),
        family.to_string(),
        (base_px * 4.0).round() as u32,
    );
    INK_CACHE.with(|c| {
        *c.borrow_mut().entry(key).or_insert_with(|| {
            text::ink_extent(seg, family, base_px).unwrap_or((base_px * 0.25, base_px * 0.72))
        })
    })
}

/// The tile's Pango font family — configured, or `"sans"` by default.
fn font_family(config: &Config) -> String {
    config
        .font_family
        .clone()
        .unwrap_or_else(|| "sans".to_string())
}

/// The `width` attribute of an embed, in logical px (falls back to `default`).
fn embed_width(attrs: &[(String, String)], default: f64) -> f64 {
    attr(attrs, "width")
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// An `<icon size="N">` multiplier of the reference cap height (default 1.0).
/// App logos want to be bigger than a digit, so the window tile sizes them up.
fn icon_size(attrs: &[(String, String)]) -> f64 {
    attr(attrs, "size")
        .and_then(|v| v.parse().ok())
        .filter(|&s: &f64| s > 0.0)
        .unwrap_or(1.0)
}

/// One element of a flow line: a text run, or a reference to an embed (by index
/// into `Processed::embeds`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum FlowItem<'a> {
    Text(&'a str),
    Embed(usize),
}

/// Split flow `markup` into lines of [`FlowItem`]s, preserving document order.
/// `markup` keeps literal `\n` and the embed placeholders in order, and
/// `Processed::embeds` is in that same order — so a single front cursor over
/// embed indices matches placeholders as they're encountered (top→bottom,
/// left→right). N placeholders split a line into N+1 segments with an embed in
/// each gap; empty segments (e.g. adjacent embeds, or a leading placeholder)
/// emit no text item.
fn flow_layout(markup: &str) -> Vec<Vec<FlowItem<'_>>> {
    let mut embed_idx = 0;
    markup
        .split('\n')
        .map(|line| {
            let segments: Vec<&str> = line.split(markup::EMBED_PLACEHOLDER).collect();
            let n_seg = segments.len();
            let mut items = Vec::new();
            for (si, seg) in segments.iter().enumerate() {
                if !seg.is_empty() {
                    items.push(FlowItem::Text(seg));
                }
                if si + 1 < n_seg {
                    items.push(FlowItem::Embed(embed_idx));
                    embed_idx += 1;
                }
            }
            items
        })
        .collect()
}

/// A laid-out text run on a flow line, with the ink metrics needed to vertically
/// center it and to size adjacent inline symbols to the glyph ink.
struct Run {
    layout: pango::Layout,
    /// Advance width (px).
    width: f64,
    /// Inked top, relative to the layout's top (px).
    ink_top: f64,
    /// Inked height (px) — for a digit run, ≈ its cap height.
    ink_h: f64,
}

/// Cap height for an inline symbol at item index `before`: the nearest preceding
/// *inked* text run's ink height (its neighbour digit), else the line's tallest
/// run, else a font-based fallback.
fn cap_at(laid: &[Option<Run>], before: usize, line_h: f64) -> f64 {
    laid[..before]
        .iter()
        .rev()
        .flatten()
        .map(|r| r.ink_h)
        .find(|&hh| hh >= 1.0)
        // No *preceding* sized run — a leading status/icon, e.g. every per-session
        // row in the multi-session tile (`<status/> folder …`). Size to a stable,
        // glyph-independent cap height so all rows' mascots match. (Previously this
        // took the max ink over the whole row, which an `↑N` unpushed badge — a
        // full-height arrow — inflated, making badged rows' mascots larger.)
        .unwrap_or(line_h * 0.62)
}

/// Reserved advance width for an embed of `tag`.
fn embed_ew(tag: &str, attrs: &[(String, String)], cap_h: f64) -> f64 {
    match tag {
        // An idle badge carrying an `ago` time needs extra width for the arc
        // that circles the bars; a plain status (dot/face/cells) is narrower.
        "status" => {
            let state = attr(attrs, "state");
            let idle_ago =
                state == Some("idle") && attr(attrs, "ago").is_some_and(|s| !s.is_empty());
            // The mascot (working/shell) is a wide sprite drawn at ~digit height,
            // so it needs a wider slot than a dot/`?`/idle cells would.
            let mult = if idle_ago {
                3.0
            } else if matches!(state, Some("working") | Some("shell")) {
                2.0
            } else {
                1.7
            };
            embed_width(attrs, cap_h * mult)
        }
        "icon" => embed_width(attrs, cap_h * icon_size(attrs) + cap_h * 0.3),
        "tickerbox" => embed_width(attrs, 160.0),
        "sep" => embed_width(attrs, cap_h * 0.4), // thin run-splitting spacer
        _ => 0.0,
    }
}

/// Compose text + inline embeds across one or more lines (see [`flow_layout`]).
/// Each run is vertically centered by its **ink box** on the line's mid-line (so
/// a big number and smaller text sit centered together, not baseline-locked), and
/// each inline symbol is sized to the ink box of its neighbouring text run and
/// centered on the same mid-line — so a dot/`?`/cells line up with the digit
/// beside them. Lines are stacked and the block vertically centered.
#[allow(clippy::too_many_arguments)]
fn draw_flow(
    cr: &gtk::cairo::Context,
    processed: &markup::Processed,
    w: f64,
    h: f64,
    config: &Config,
    style: &text::TextStyle,
    time: f32,
    scale: f64,
    mut live: Option<&mut Segments>,
) {
    // Rebound when `live` cuts a new segment, so later drawing lands on it.
    let mut cr = cr.clone();
    let cr = &mut cr;
    let line_h = config.font_size as f64 * 1.4;
    let pad = config.font_size as f64 * 0.8; // L/R pad — also the active card's inner margin
    let center = config.align.as_deref() == Some("center");
    let lines = flow_layout(&processed.markup);

    // A leading `<icon hero/>` or `<gutter>…</gutter>` becomes a big left gutter;
    // the text lines indent past it. Lets a window tile show a prominent app icon,
    // or a multi-session tile show one big shared shortcut, with the rows to its
    // right. Detected here (so `left` feeds the wrap width) but drawn after the
    // block height is known, so it centers on the whole content block.
    let mut left = pad;
    // Whether a leading gutter element occupies line0[0].
    let mut hero = false;
    // Dark glow behind every text run, for contrast against a translucent bar (or
    // a watermark icon). Always on so all tiles' numbers and titles read crisply.
    let halo = true;
    let halo_w = config.font_size as f64 * 0.12;
    let mut hero_draw: Option<(&markup::Embed, f64)> = None;
    let mut watermark_draw: Option<(&markup::Embed, f64)> = None;
    // (layout, markup, ink_top, ink_h)
    let mut gutter_draw: Option<(pango::Layout, &str, f64, f64)> = None;
    if let Some(FlowItem::Embed(idx)) = lines.first().and_then(|l| l.first()) {
        if let Some(e) = processed.embeds.get(*idx) {
            if e.tag == "icon" && attr(&e.attrs, "watermark").is_some() {
                // A big, dimmed app icon behind the text (centred, not a gutter).
                // Text uses the full width (normal pads); the icon shows through
                // wherever the text doesn't cover it. A dark halo behind the text
                // restores contrast over the artwork.
                let wm_h = h * 0.92;
                left = pad;
                hero = true; // skip the leading embed in the per-line pass
                watermark_draw = Some((e, wm_h));
            } else if e.tag == "icon" && attr(&e.attrs, "hero").is_some() {
                let hero_h = h * 0.7;
                left = pad + hero_h + config.font_size as f64 * 0.5;
                hero = true;
                hero_draw = Some((e, hero_h));
            } else if e.tag == "gutter" {
                // Big text gutter (e.g. a shared shortcut digit). Its size comes
                // from the inner markup's span; we measure it to set the indent.
                let (layout, _oy, gw) = text::layout_line(cr, &e.inner, line_h, style);
                // Cached (run_ink): ink_extent rasterizes + scans, and this runs
                // every frame on an animating dual tile — uncached, it crawls.
                let (ink_top, ink_h) =
                    run_ink(&e.inner, &style.font_family, config.font_size as f64);
                left = pad + gw + config.font_size as f64 * 0.5;
                hero = true;
                gutter_draw = Some((layout, &e.inner, ink_top, ink_h));
            }
        }
    }

    // Per-line heights: a `<wrap>` block word-wraps to the remaining width and is
    // as tall as it needs; every other line is one `line_h`. Pre-compute (and
    // keep the wrapped layouts) so the whole block can be vertically centered.
    let avail = (w - left - pad).max(20.0);
    let mut wraps: Vec<Option<(pango::Layout, &str)>> = Vec::with_capacity(lines.len());
    let mut heights: Vec<f64> = Vec::with_capacity(lines.len());
    for items in &lines {
        let wrap = items.iter().find_map(|it| match *it {
            FlowItem::Embed(i) => processed.embeds.get(i).filter(|e| e.tag == "wrap"),
            _ => None,
        });
        match wrap {
            Some(e) => {
                let layout = text::layout_wrapped(cr, &e.inner, avail, style);
                let hgt = (layout.pixel_size().1 as f64).max(line_h);
                wraps.push(Some((layout, &e.inner)));
                heights.push(hgt);
            }
            None => {
                wraps.push(None);
                heights.push(line_h);
            }
        }
    }
    let total: f64 = heights.iter().sum();
    // Top-align the block (a small, tile-independent top pad) so every tile's
    // header sits on the same line regardless of how many wrapped lines follow.
    // (Tall content that exceeds the tile just starts at the pad and clips.)
    let top_pad = config.font_size as f64 * 0.5;
    let mut y = top_pad;

    // Draw the dimmed background watermark first, so text (and its halo) land on
    // top. Centred vertically; horizontally biased toward the right so the
    // left-aligned text overlays its left half and the right half stays clear.
    if let Some((e, wm_h)) = watermark_draw {
        let cx = (w * 0.60).min(w - wm_h * 0.5 - pad * 0.5);
        draw_icon_alpha(cr, &e.attrs, cx, h / 2.0, wm_h, scale, 0.22);
    }
    // Draw the deferred hero icon, centered on the content block (clamped to stay
    // within the tile) so it reads as part of the now top-aligned content.
    if let Some((e, hero_h)) = hero_draw {
        let cy = (top_pad + total / 2.0).clamp(hero_h / 2.0, h - hero_h / 2.0);
        draw_icon(cr, &e.attrs, pad + hero_h / 2.0, cy, hero_h, scale);
    }
    // Big text gutter, ink-centered on the content block at the left pad.
    if let Some((layout, inner, ink_top, ink_h)) = &gutter_draw {
        let cy = top_pad + total / 2.0;
        let gy = cy - ink_top - ink_h / 2.0;
        if halo {
            text::paint_haloed(cr, layout, inner, pad, gy, halo_w, style, scale);
        } else {
            text::paint(cr, layout, pad, gy, style);
        }
    }

    for (li, items) in lines.iter().enumerate() {
        let lh = heights[li];
        let ly = y;
        let center_y = y + lh / 2.0; // the line's vertical mid-line
        y += lh;

        // A wrapped body line: paint the multi-line block at the gutter, top-down.
        if let Some((layout, inner)) = &wraps[li] {
            if halo {
                text::paint_haloed(cr, layout, inner, left, ly, halo_w, style, scale);
            } else {
                text::paint(cr, layout, left, ly, style);
            }
            continue;
        }

        // First pass: lay out each text run, capturing its ink box.
        let mut laid: Vec<Option<Run>> = Vec::with_capacity(items.len());
        for item in items {
            match *item {
                FlowItem::Text(seg) => {
                    let (layout, _oy, width) = text::layout_line(cr, seg, line_h, style);
                    // True rendered ink (font-robust, incl. bitmap fonts), cached.
                    let (ink_top, ink_h) =
                        run_ink(seg, &style.font_family, config.font_size as f64);
                    laid.push(Some(Run {
                        layout,
                        width,
                        ink_top,
                        ink_h,
                    }));
                }
                FlowItem::Embed(_) => laid.push(None),
            }
        }

        // Total advance of this line, so it can be centered within the tile.
        let mut total = 0.0;
        for (ii, item) in items.iter().enumerate() {
            match *item {
                FlowItem::Text(_) => {
                    if let Some(run) = &laid[ii] {
                        total += run.width;
                    }
                }
                FlowItem::Embed(idx) => {
                    if let Some(embed) = processed.embeds.get(idx) {
                        total += embed_ew(&embed.tag, &embed.attrs, cap_at(&laid, ii, line_h));
                    }
                }
            }
        }

        // Second pass: place runs (ink box centered on the mid-line) and symbols
        // (sized to the neighbour digit, centered on the mid-line).
        let mut x = if center {
            ((w - total) / 2.0).max(0.0)
        } else {
            left
        };
        for (ii, item) in items.iter().enumerate() {
            // Skip the leading hero icon — it's already drawn as the gutter.
            if hero && li == 0 && ii == 0 {
                continue;
            }
            match *item {
                FlowItem::Text(seg) => {
                    if let Some(run) = &laid[ii] {
                        let y = center_y - run.ink_top - run.ink_h / 2.0;
                        if halo {
                            text::paint_haloed(cr, &run.layout, seg, x, y, halo_w, style, scale);
                        } else {
                            text::paint(cr, &run.layout, x, y, style);
                        }
                        x += run.width;
                    }
                }
                FlowItem::Embed(idx) => {
                    let Some(embed) = processed.embeds.get(idx) else {
                        continue;
                    };
                    let cap_h = cap_at(&laid, ii, line_h);
                    let ew = embed_ew(&embed.tag, &embed.attrs, cap_h);
                    // Time-driven embeds go to `live` when the caller is
                    // caching the static layer; otherwise draw them in place.
                    let op = match embed.tag.as_str() {
                        "status" => Some(LiveOp::Status {
                            attrs: embed.attrs.clone(),
                            cx: x + ew / 2.0,
                            cy: center_y,
                            cap_h,
                        }),
                        // A ticker whose text fits never scrolls: draw it now,
                        // so a cached layer bakes it in instead of redrawing it.
                        "tickerbox" if ticker_fits(cr, &embed.inner, ew, line_h, config) => {
                            draw_ticker(cr, &embed.inner, (x, ly, ew, line_h), config, time);
                            None
                        }
                        "tickerbox" => Some(LiveOp::Ticker {
                            inner: embed.inner.clone(),
                            rect: (x, ly, ew, line_h),
                        }),
                        "icon" => {
                            let isz = cap_h * icon_size(&embed.attrs);
                            draw_icon(cr, &embed.attrs, x + ew / 2.0, center_y, isz, scale);
                            None
                        }
                        _ => None,
                    };
                    if let Some(op) = op {
                        match live.as_deref_mut() {
                            Some(segs) => match segs.cut(op) {
                                Ok(next) => *cr = next,
                                // No memory for a new raster: bake this frame's
                                // pose in rather than lose the element.
                                Err(op) => op.draw(cr, config, &style.font_family, time, scale),
                            },
                            None => op.draw(cr, config, &style.font_family, time, scale),
                        }
                    }
                    x += ew;
                }
            }
        }
    }
}

/// Value of attribute `key`, if present.
fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Whole-tile `<pulse>` oscillation period, in seconds (the prompt attention blink).
const PULSE_PERIOD: f64 = 1.3;

/// Blink/pulse multiplier in `[lo, 1.0]`, a sine oscillating with `period` secs.
fn osc(time: f32, period: f64, lo: f64) -> f64 {
    let phase = (time as f64 / period) * std::f64::consts::TAU;
    let s = 0.5 + 0.5 * phase.sin(); // 0..1
    lo + (1.0 - lo) * s
}

/// Per-level idle colour: a fade from bright white (just-idled) to dim grey
/// (long idle), matching the daemon's 7-step decay.
const IDLE_LEVELS: [&str; 7] = [
    "#ffffff", "#d8d8d8", "#b0b0b0", "#888888", "#686868", "#505050", "#3a3a3a",
];

/// Slow glow behind a recently-idle indicator — so the decay cells read as
/// "recently here", not a dead pause symbol. Coloured like the cells (white when
/// fresh), it pulses slowly, fades as idleness grows, and switches off entirely
/// past the one-hour mark (the dimmest level), where the tile goes static again.
const IDLE_GLOW_PERIOD: f64 = 3.4; // slow blink, seconds
const IDLE_GLOW_MAX: f64 = 0.6; // peak alpha when freshly idle
const IDLE_GLOW_FLOOR: f64 = 0.45; // mid-blink floor — stays a glow, not a flash

/// Set the Cairo source to a hex colour, multiplying its alpha by `alpha_mul`.
fn set_hex(cr: &gtk::cairo::Context, hex: &str, alpha_mul: f64) {
    if let Some(c) = render::parse_hex_color(hex) {
        cr.set_source_rgba(c.r as f64, c.g as f64, c.b as f64, c.a as f64 * alpha_mul);
    }
}

/// Draw the claude-session indicator for `<status state=".." level="N"/>`, sized
/// to the neighbouring digit (`cap_h` = the digit's ink height) and centered on
/// the line mid-line `cy` at slot centre `cx`. States: blinking orange dot
/// (`working`), pulsing cyan dot (`shell`), a bright `?` over a yellow bloom
/// (`prompt`), or a static two-cell fade bar (`idle`).
#[allow(clippy::too_many_arguments)]
fn draw_status(
    cr: &gtk::cairo::Context,
    attrs: &[(String, String)],
    cx: f64,
    cy: f64,
    cap_h: f64,
    family: &str,
    time: f32,
    scale: f64,
) {
    if cap_h < 1.0 {
        return;
    }
    let state = attr(attrs, "state").unwrap_or("idle");
    let r = cap_h * 0.5;
    match state {
        // Slow, deep blink/pulse (wide amplitude) — a "bursting bubble", not a
        // twitch: a color-matched glow swells under the iconic Claude robot face,
        // both riding the same oscillation. Electric hues; the colour codes the
        // state (orange = working, cyan = shell).
        "working" => {
            let a = osc(time, 2.0, 0.15);
            glow_halo(cr, cx, cy, cap_h * 1.25, "#ff5a14", a * 0.5);
            draw_status_face(cr, cx, cy, cap_h * 1.15, "#ff5a14", a, scale);
        }
        "shell" => {
            let a = osc(time, 2.8, 0.25);
            glow_halo(cr, cx, cy, cap_h * 1.2, "#12f5ff", a * 0.4);
            draw_status_face(cr, cx, cy, cap_h * 1.15, "#12f5ff", a, scale);
        }
        // A bright `?` (drawn at the digit's scale, centered) over a visible
        // yellow bloom. The blink comes from the whole-tile `<pulse>`.
        "prompt" => {
            // Softer halo, but a saturated glyph so the `?` reads crisply on it.
            glow_halo(cr, cx, cy, cap_h * 1.3, "#f9e2af", 0.55);
            draw_glyph_centered(cr, "?", cx, cy, cap_h, family, "#ffd42a");
        }
        "idle" => {
            let level = attr(attrs, "level")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0)
                .min(IDLE_LEVELS.len() - 1);
            let ago = attr(attrs, "ago").unwrap_or("");
            // Slow glow for the first hour (levels 0..=5), fading with age; off at
            // the dimmest level. Coloured like the cells, so it's white when fresh.
            let last = IDLE_LEVELS.len() - 1;
            let glow_a = if level < last {
                // Gentle fade across the hour (1.0 → ~0.5), not a cliff — so it
                // glows fairly evenly through the first hour, off at the last level.
                let recency = 1.0 - 0.5 * level as f64 / last as f64;
                osc(time, IDLE_GLOW_PERIOD, IDLE_GLOW_FLOOR) * IDLE_GLOW_MAX * recency
            } else {
                0.0
            };
            if ago.is_empty() {
                draw_idle_cells(cr, cx, cy, cap_h, IDLE_LEVELS[level], glow_a);
            } else {
                draw_idle_badge(cr, cx, cy, cap_h, IDLE_LEVELS[level], ago, glow_a);
            }
        }
        // An empty desktop: a dim hollow ring (no activity), digit-sized.
        "empty" => draw_ring(cr, cx, cy, r, "#6c7086"),
        _ => {}
    }
}

/// Draw a hollow ring (outline circle) of `hex` at `(cx, cy)`, radius `r`.
fn draw_ring(cr: &gtk::cairo::Context, cx: f64, cy: f64, r: f64, hex: &str) {
    set_hex(cr, hex, 1.0);
    cr.set_line_width((r * 0.30).max(1.0));
    cr.new_sub_path(); // detach from any leftover current point (else arc() connects)
    cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
}

/// The "focus bubble": the rounded-rect inset within a `w`×`h` tile, as `(x, y,
/// w, h, radius)` in logical px. It's the shape of the active-desktop card AND
/// the mask a `<bg>` shader layer is clipped to — one definition so they always
/// agree. The rect is centred (equal insets), which lets the shader compute its
/// mask from `gl_FragCoord` regardless of GL's y-origin.
fn focus_bubble(w: f64, h: f64) -> (f64, f64, f64, f64, f64) {
    let inset = 2.0;
    let pw = (w - 2.0 * inset).max(0.0);
    let ph = (h - 2.0 * inset).max(0.0);
    (inset, inset, pw, ph, ph * 0.20)
}

/// Draw the active-desktop accent: a rounded "card" — a faint accent fill plus a
/// brighter accent border — on the focus bubble, behind the content.
fn draw_active_panel(cr: &gtk::cairo::Context, w: f64, h: f64) {
    let (x, y, pw, ph, r) = focus_bubble(w, h);
    rounded_rect(cr, x, y, pw, ph, r);
    set_hex(cr, "#89b4fa", 0.12);
    let _ = cr.fill();
    rounded_rect(cr, x, y, pw, ph, r);
    set_hex(cr, "#89b4fa", 0.75);
    cr.set_line_width(1.5);
    let _ = cr.stroke();
}

/// Default opacity of a `<bg>` shader layer — mild by design (a graphical accent,
/// not a full background). Overridable per-tile with `<bg … alpha="…">`.
const BG_ALPHA: f32 = 0.28;
/// Default edge-fade width (logical px) of the `<bg>` focus-bubble mask.
const BG_FADE: f32 = 20.0;

/// Render a `<bg preset="…">` shader layer, masked to the focus bubble at moderate
/// alpha, and composite it behind the tile content. Reserved attrs `alpha`/`fade`
/// tune the mask; any other `name="float"` attr becomes a shader uniform.
fn draw_bg(cr: &gtk::cairo::Context, w: f64, h: f64, bg: &markup::BgSpec, fx: &mut EffectCtx) {
    let Some(name) = bg.preset() else { return };
    let Some(src) = shader::preset(name) else {
        eprintln!("pwetty-box: unknown bg preset '{name}'");
        return;
    };
    let dw = (w * fx.scale).round() as i32;
    let dh = (h * fx.scale).round() as i32;
    if dw <= 0 || dh <= 0 {
        return;
    }
    let s = fx.scale as f32;
    let (bx, by, bw, bh, r) = focus_bubble(w, h);
    // Defaults first; per-tile attrs pushed after override by name (last wins in
    // the GL uniform set).
    let mut uniforms: Vec<(String, f32)> = vec![
        ("u_bx".into(), bx as f32 * s),
        ("u_by".into(), by as f32 * s),
        ("u_bw".into(), bw as f32 * s),
        ("u_bh".into(), bh as f32 * s),
        ("u_radius".into(), r as f32 * s),
        ("u_fade".into(), BG_FADE * s),
        // Slow vignette radius: default to the bubble's short half-extent so the
        // layer is brightest deep inside and gently fades out toward the edges,
        // independent of (and stacked under) the steep `u_fade` edge cliff.
        ("u_falloff".into(), (bw.min(bh) * 0.5) as f32 * s),
        ("u_alpha".into(), BG_ALPHA),
    ];
    for (k, v) in &bg.attrs {
        match k.as_str() {
            "preset" => {}
            "alpha" => {
                if let Ok(f) = v.parse() {
                    uniforms.push(("u_alpha".into(), f));
                }
            }
            "fade" => {
                if let Ok(f) = v.parse::<f32>() {
                    uniforms.push(("u_fade".into(), f * s));
                }
            }
            "falloff" => {
                if let Ok(f) = v.parse::<f32>() {
                    uniforms.push(("u_falloff".into(), f * s));
                }
            }
            // A hex colour expands to three `name_r/g/b` float uniforms (so a
            // preset can take e.g. stars="#f9e2af"); otherwise a plain float.
            _ => {
                if let Some(c) = render::parse_hex_color(v) {
                    uniforms.push((format!("{k}_r"), c.r));
                    uniforms.push((format!("{k}_g"), c.g));
                    uniforms.push((format!("{k}_b"), c.b));
                } else if let Ok(f) = v.parse::<f32>() {
                    uniforms.push((k.clone(), f));
                }
            }
        }
    }
    let key = format!("bg:{name}");
    if let Some(rgba) = fx
        .shaders
        .render_masked(&key, src, dw, dh, fx.time, fx.frame, &uniforms)
    {
        paint_rgba_at(cr, dw as usize, dh as usize, rgba, fx.scale, 0.0, 0.0);
    }
}

/// Rasterized icons keyed by (source, device px, tint) — a ready-to-paint
/// surface (cheap to clone: refcounted), or `None` for a cached failure.
type IconCache = HashMap<(String, u32, Option<u32>), Option<gtk::cairo::ImageSurface>>;

thread_local! {
    /// Per-thread icon raster cache, so animated tiles don't re-rasterize an SVG
    /// every frame.
    static ICON_CACHE: RefCell<IconCache> = RefCell::new(HashMap::new());
}

/// Draw an `<icon name="…"/>` (bundled) or `<icon src="path.svg"/>` (file),
/// sized to `cap_h` (the neighbour digit) and centered on `(cx, cy)`. An optional
/// `color` attribute tints the SVG (a monochrome silhouette); without it, the
/// artwork's own colours render (e.g. an app logo). Rasterized at device
/// resolution and cached.
fn draw_icon(
    cr: &gtk::cairo::Context,
    attrs: &[(String, String)],
    cx: f64,
    cy: f64,
    cap_h: f64,
    scale: f64,
) {
    draw_icon_alpha(cr, attrs, cx, cy, cap_h, scale, 1.0);
}

/// As [`draw_icon`], but composited at `alpha` — used to dim an icon when it's
/// drawn as a faint background watermark behind text.
#[allow(clippy::too_many_arguments)]
fn draw_icon_alpha(
    cr: &gtk::cairo::Context,
    attrs: &[(String, String)],
    cx: f64,
    cy: f64,
    cap_h: f64,
    scale: f64,
    alpha: f64,
) {
    if cap_h < 1.0 {
        return;
    }
    let name = attr(attrs, "name");
    let src = attr(attrs, "src");
    let key = match (name, src) {
        (Some(n), _) => format!("name:{n}"),
        (_, Some(s)) => format!("src:{s}"),
        _ => return,
    };
    let tint = attr(attrs, "color")
        .and_then(render::parse_hex_color)
        .map(|c| (c.r, c.g, c.b));
    let px = ((cap_h * scale).round() as u32).max(1);

    let buf = raster_svg_cached(key, px, tint, || match (name, src) {
        (Some(n), _) => svg::bundled(n).map(|s| s.as_bytes().to_vec()),
        (_, Some(s)) => std::fs::read(s).ok(),
        _ => None,
    });
    if let Some(surface) = buf {
        paint_surface_at(
            cr,
            &surface,
            scale,
            cx - cap_h / 2.0,
            cy - cap_h / 2.0,
            alpha,
        );
    }
}

/// Composite a device-resolution `surface` at logical `(ox, oy)` and `alpha`,
/// scaling device pixels back to logical. Like [`paint_rgba_at`] but for data
/// already in Cairo's format (e.g. a cached SVG raster).
fn paint_surface_at(
    cr: &gtk::cairo::Context,
    surface: &gtk::cairo::ImageSurface,
    device_scale: f64,
    ox: f64,
    oy: f64,
    alpha: f64,
) {
    let _ = cr.save();
    cr.translate(ox, oy);
    let s = 1.0 / device_scale;
    cr.scale(s, s);
    if cr.set_source_surface(surface, 0.0, 0.0).is_ok() {
        let _ = cr.paint_with_alpha(alpha);
    }
    let _ = cr.restore();
}

/// Rasterize `key`'s SVG to a `px` square, optionally `tint`ed, cached by
/// (key, px, tint). `load` supplies the SVG bytes on a cache miss.
fn raster_svg_cached(
    key: String,
    px: u32,
    tint: Option<(f32, f32, f32)>,
    load: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<gtk::cairo::ImageSurface> {
    use gtk::cairo::{Format, ImageSurface};
    let tint_key = tint.map(|(r, g, b)| {
        ((r * 255.0) as u32) << 16 | ((g * 255.0) as u32) << 8 | (b * 255.0) as u32
    });
    ICON_CACHE.with(|c| {
        c.borrow_mut()
            .entry((key, px, tint_key))
            .or_insert_with(|| {
                let data = load().and_then(|b| svg::rasterize_argb32(&b, px, tint))?;
                let (side, stride) = (px as i32, 4 * px as i32);
                ImageSurface::create_for_data(data, Format::ARgb32, side, side, stride).ok()
            })
            .clone()
    })
}

/// Draw the bundled `claude-face` mascot, tinted to `hex`, so its **ink height**
/// is `height` and it's centered at `(cx, cy)`, composited at `alpha` (so it
/// blinks/pulses with the status oscillation). Rasterized at device resolution
/// and cached.
///
/// The mascot is wider than tall (banner sprite, ~18×11), and `rasterize_argb32`
/// fits it into a *square* buffer — so a square side of `height` would render the
/// creature at only `11/18` of that height and read tiny next to the digit. We
/// size the square box by the sprite's aspect so the painted ink height matches
/// `height` (≈ the neighbouring digit's cap height).
fn draw_status_face(
    cr: &gtk::cairo::Context,
    cx: f64,
    cy: f64,
    height: f64,
    hex: &str,
    alpha: f64,
    scale: f64,
) {
    // claude-face viewBox aspect (width / height). Keep in sync with the SVG.
    const MASCOT_ASPECT: f64 = 18.0 / 11.0;
    let box_side = height * MASCOT_ASPECT;
    let px = ((box_side * scale).round() as u32).max(1);
    let tint = render::parse_hex_color(hex).map(|c| (c.r, c.g, c.b));
    if let Some(surface) = raster_svg_cached("name:claude-face".to_string(), px, tint, || {
        svg::bundled("claude-face").map(|s| s.as_bytes().to_vec())
    }) {
        let (ox, oy) = (cx - box_side / 2.0, cy - box_side / 2.0);
        paint_surface_at(cr, &surface, scale, ox, oy, alpha);
    }
}

/// Paint a soft radial bloom of `hex` (peak `alpha` at the centre, fading to 0 at
/// `radius`) centred at `(cx, cy)` — a CPU glow that can bleed past its slot.
fn glow_halo(cr: &gtk::cairo::Context, cx: f64, cy: f64, radius: f64, hex: &str, alpha: f64) {
    let Some(c) = render::parse_hex_color(hex) else {
        return;
    };
    if alpha <= 0.0 || radius <= 0.0 {
        return;
    }
    // Animated glows only vary `alpha`, which scales every stop uniformly — the
    // same as painting a full-alpha bloom at `alpha`. So rasterize the bloom
    // once per (colour, radius, sub-pixel phase) and blit it; a per-frame
    // gradient fill was ~40% of a cached claude tile's frame.
    // In the target's real device pixels — CTM and surface device scale (GTK's
    // HiDPI) — so the bloom is rasterized at full resolution.
    let Some(scale) = text::device_scale(cr) else {
        // A transform the cache can't reproduce: fill the gradient directly.
        let rgb = (c.r as f64, c.g as f64, c.b as f64);
        let grad = gtk::cairo::RadialGradient::new(cx, cy, 0.0, cx, cy, radius);
        grad.add_color_stop_rgba(0.0, rgb.0, rgb.1, rgb.2, alpha);
        grad.add_color_stop_rgba(0.45, rgb.0, rgb.1, rgb.2, alpha * 0.45);
        grad.add_color_stop_rgba(1.0, rgb.0, rgb.1, rgb.2, 0.0);
        if cr.set_source(&grad).is_ok() {
            cr.new_sub_path();
            cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
        return;
    };
    let (dev_x, dev_y) = text::to_device(cr, cx, cy);
    let dev_r = radius * scale;
    let phase = |v: f64| ((v - v.floor()) * 4.0).round() / 4.0;
    let (px, py) = (phase(dev_x), phase(dev_y));
    let q = |v: f64| (v * 64.0).round() as u32;
    let rgb = ((c.r * 255.0) as u32) << 16 | ((c.g * 255.0) as u32) << 8 | (c.b * 255.0) as u32;
    let key = (rgb, q(dev_r), q(px), q(py));
    let bloom = BLOOM_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= BLOOM_CACHE_MAX && !cache.contains_key(&key) {
            cache.clear();
        }
        cache
            .entry(key)
            .or_insert_with(|| raster_bloom((c.r as f64, c.g as f64, c.b as f64), dev_r, (px, py)))
            .clone()
    });
    let Some((surface, off)) = bloom else { return };
    let origin = ((dev_x - px).round(), (dev_y - py).round());
    text::blit_at_device_alpha(cr, &surface, origin, (-off, -off), scale, alpha);
}

/// Cached full-alpha blooms: (rgb, device radius×64, phase x×64, phase y×64) →
/// (surface, whole-px offset from the centre's integer pixel to its top-left).
type BloomCache = HashMap<(u32, u32, u32, u32), Option<(gtk::cairo::ImageSurface, f64)>>;

/// Blooms beyond this drop the cache (sizes track font/scale, so it stays tiny).
const BLOOM_CACHE_MAX: usize = 64;

thread_local! {
    static BLOOM_CACHE: RefCell<BloomCache> = RefCell::new(HashMap::new());
}

/// Rasterize [`glow_halo`]'s bloom at full alpha, `r` device px in radius, its
/// centre at sub-pixel `phase` within its integer pixel.
fn raster_bloom(
    (r, g, b): (f64, f64, f64),
    radius: f64,
    phase: (f64, f64),
) -> Option<(gtk::cairo::ImageSurface, f64)> {
    use gtk::cairo::{Context, Format, ImageSurface, RadialGradient};
    let off = radius.ceil() + 1.0;
    let side = (2.0 * off + 1.0) as i32;
    let surface = ImageSurface::create(Format::ARgb32, side, side).ok()?;
    {
        let cr = Context::new(&surface).ok()?;
        let (cx, cy) = (off + phase.0, off + phase.1);
        let grad = RadialGradient::new(cx, cy, 0.0, cx, cy, radius);
        grad.add_color_stop_rgba(0.0, r, g, b, 1.0);
        grad.add_color_stop_rgba(0.45, r, g, b, 0.45);
        grad.add_color_stop_rgba(1.0, r, g, b, 0.0);
        cr.set_source(&grad).ok()?;
        cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
        cr.fill().ok()?;
    }
    surface.flush();
    Some((surface, off))
}

/// Draw a bold `glyph` (e.g. `?`) sized so its ink height ≈ `cap_h`, with its ink
/// box centered on `(cx, cy)` — so it aligns like an adjacent centered digit.
fn draw_glyph_centered(
    cr: &gtk::cairo::Context,
    glyph: &str,
    cx: f64,
    cy: f64,
    cap_h: f64,
    family: &str,
    hex: &str,
) {
    let c = render::parse_hex_color(hex).unwrap_or(femtovg_white());
    let style = text::TextStyle {
        font_family: family.to_string(),
        size_px: cap_h / 0.70, // ? cap ≈ 0.7·em, so this gives ink height ≈ cap_h
        color: (c.r as f64, c.g as f64, c.b as f64, 1.0),
        align_center: false,
    };
    let markup = format!("<b>{glyph}</b>");
    let (layout, _oy, lw) = text::layout_line(cr, &markup, cap_h * 2.0, &style);
    // Use the TRULY-rendered ink (font-robust, incl. bitmap fonts) to center —
    // the same metric the neighbouring digit uses, so they line up. Pango's
    // pixel_extents lie for bitmap fonts and would misalign the glyph.
    // Cached (run_ink): this runs every frame for the blinking prompt '?'.
    let (ink_top, ink_h) = run_ink(&markup, family, style.size_px);
    let y = cy - ink_top - ink_h / 2.0;
    text::paint(cr, &layout, cx - lw / 2.0, y, &style);
}

/// Draw the idle indicator: two rounded cells of height `cap_h` in `hex` (the
/// level colour), centered at `(cx, cap_cy)` — evoking the `██`/`░░` decay bar,
/// sized to the digit beside it.
fn draw_idle_cells(
    cr: &gtk::cairo::Context,
    cx: f64,
    cap_cy: f64,
    cap_h: f64,
    hex: &str,
    glow_a: f64,
) {
    // A soft glow behind the cells (recently-idle cue) — always WHITE, never the
    // cell colour: the cells grey out within minutes and a grey glow on a dark bar
    // is invisible. White stays visible across the whole hour; only the alpha
    // fades. Wide radius so it reads as a halo even when the cells are small.
    if glow_a > 0.001 {
        glow_halo(cr, cx, cap_cy, cap_h * 1.9, "#ffffff", glow_a);
    }
    let cw = cap_h * 0.42;
    let gap = cap_h * 0.24;
    let total = cw * 2.0 + gap;
    let x0 = cx - total / 2.0;
    let y0 = cap_cy - cap_h / 2.0;
    set_hex(cr, hex, 1.0);
    rounded_rect(cr, x0, y0, cw, cap_h, cw * 0.3);
    let _ = cr.fill();
    rounded_rect(cr, x0 + cw + gap, y0, cw, cap_h, cw * 0.3);
    let _ = cr.fill();
}

/// Idle age in minutes, parsed from a short `ago` label like `45s` / `12m` / `2h`.
fn idle_minutes(ago: &str) -> f64 {
    let s = ago.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n: f64 = s[..split].parse().unwrap_or(0.0);
    match s[split..].trim() {
        "h" | "hr" | "hrs" => n * 60.0,
        "d" => n * 24.0 * 60.0,
        "s" | "sec" => n / 60.0,
        _ => n, // "m"/minutes, and the common bare-number case
    }
}

/// Idle time-label colour by age — a piecewise ramp that recedes as a session
/// sits longer untouched:
///   <5 min  full white
///   1 h     full red        (white→red across 5..60 min)
///   3 h     full red        (holds red through 1..3 h)
///   6 h     bright purple   (red→bright purple across 3..6 h)
///   9 h+    dark purple     (bright→dark purple across 6..9 h, then holds)
fn idle_age_color(ago: &str) -> (f64, f64, f64) {
    // (minutes, r, g, b) keyframes, ascending; interpolated linearly, clamped
    // at both ends.
    const KF: [(f64, f64, f64, f64); 5] = [
        (5.0, 1.0, 1.0, 1.0),      // white
        (60.0, 1.0, 0.0, 0.0),     // red
        (180.0, 1.0, 0.0, 0.0),    // red (held)
        (360.0, 0.78, 0.30, 1.0),  // bright purple
        (540.0, 0.30, 0.08, 0.45), // dark purple
    ];
    let m = idle_minutes(ago);
    let first = KF[0];
    let last = KF[KF.len() - 1];
    if m <= first.0 {
        return (first.1, first.2, first.3);
    }
    if m >= last.0 {
        return (last.1, last.2, last.3);
    }
    for w in KF.windows(2) {
        let (m0, r0, g0, b0) = w[0];
        let (m1, r1, g1, b1) = w[1];
        if m <= m1 {
            let t = (m - m0) / (m1 - m0);
            return (r0 + (r1 - r0) * t, g0 + (g1 - g0) * t, b0 + (b1 - b0) * t);
        }
    }
    (last.1, last.2, last.3)
}

/// Draw the idle indicator as a **badge**: the two decay cells on the left, with
/// the "time since active" (`ago`, e.g. `12m`) as a plain, readable, *horizontal*
/// label to their right — vertically centred on the line. The label is a flat
/// colour that fades with idle age (see [`idle_age_color`]): white when fresh,
/// to red by 1 h, then on through bright purple (~6 h) to dark purple (9 h+) —
/// mirroring the cells' fade-to-black, but in colour. No gradient/animation, so
/// idle tiles stay cheap. The whole badge lives in the status embed's reserved
/// width, so the time never spills into the folder/next tile.
fn draw_idle_badge(
    cr: &gtk::cairo::Context,
    cx: f64,
    cy: f64,
    cap_h: f64,
    hex: &str,
    ago: &str,
    glow_a: f64,
) {
    // The box is `cap_h * 3.0` wide (see `embed_ew`), centred on `cx`. Put the
    // bars at the left, the time filling the rest.
    let ew = cap_h * 3.0;
    let left = cx - ew / 2.0;
    let bar_cx = left + cap_h * 0.55;
    draw_idle_cells(cr, bar_cx, cy, cap_h, hex, glow_a);

    let fs = cap_h * 0.92; // big enough to actually read at tile scale
    if fs < 4.0 || ago.is_empty() {
        return;
    }
    cr.save().ok();
    cr.select_font_face(
        "monospace",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Bold,
    );
    cr.set_font_size(fs);
    let tx = bar_cx + cap_h * 0.85; // just right of the cells
    let ty = cy + fs * 0.36; // baseline -> roughly vertically centred ink
                             // Flat colour by age (no gradient): white→red→purple
                             // as the session sits longer untouched.
    let (r, g, b) = idle_age_color(ago);
    cr.set_source_rgb(r, g, b);
    cr.move_to(tx, ty);
    let _ = cr.show_text(ago);
    cr.restore().ok();
}

/// Opaque white femtovg colour (fallback for an unparsable hex).
fn femtovg_white() -> femtovg::Color {
    femtovg::Color::rgb(255, 255, 255)
}

/// Logical pixels/second the ticker scrolls (brisk but comfortable to read).
const TICKER_SPEED: f64 = 70.0;

/// Render `inner` markup within the embed `rect`. If it fits, draw it statically
/// (no scroll). If it's wider than the box, scroll it right-to-left with a `◆`
/// marker, clipped, looping — but with a clearing gap so the box goes fully
/// blank for a beat before the next pass enters (less nauseating than a seamless
/// loop). `time` is seconds since start.
fn draw_ticker(
    cr: &gtk::cairo::Context,
    inner: &str,
    rect: text::Rect,
    config: &Config,
    time: f32,
) {
    let (rx, ry, rw, rh) = rect;
    if rw < 1.0 || rh < 1.0 {
        return;
    }
    let style = text::TextStyle {
        font_family: font_family(config),
        size_px: config.font_size as f64,
        color: (0.95, 0.95, 1.0, 1.0),
        align_center: false,
    };

    let _ = cr.save();
    cr.rectangle(rx, ry, rw, rh);
    cr.clip();

    // Measure the bare text. If it fits the box, no need to scroll — render it
    // statically so short titles don't pointlessly loop.
    let (text_layout, toy, tw) = text::layout_line(cr, inner, rh, &style);
    if tw <= rw {
        text::paint(cr, &text_layout, rx, ry + toy, &style);
        let _ = cr.restore();
        return;
    }

    // Too wide: scroll. Loop unit = text + a `◆` marker; the loop period adds a
    // clearing gap of ~1.25× the box width so the box empties between passes.
    // The unit's text never changes while it scrolls, so it's painted from the
    // text raster cache (no halo) rather than re-rasterized every frame.
    let unit = format!("{inner}   <span foreground=\"#89b4fa\">\u{25c6}</span>");
    let (layout, oy, uw) = text::layout_line(cr, &unit, rh, &style);
    let period = uw + rw * 1.25;
    let offset = (time as f64 * TICKER_SPEED).rem_euclid(period);
    let scale = cr.user_to_device_distance(1.0, 0.0).map_or(1.0, |(d, _)| d);
    let mut x = rx - offset;
    while x < rx + rw {
        text::paint_haloed(cr, &layout, &unit, x, ry + oy, 0.0, &style, scale);
        x += period;
    }
    let _ = cr.restore();
}

/// Whether a `<tickerbox>` with `inner` markup fits its `width`×`height` box —
/// a ticker that fits never scrolls, so it can be drawn once, statically.
fn ticker_fits(
    cr: &gtk::cairo::Context,
    inner: &str,
    width: f64,
    height: f64,
    config: &Config,
) -> bool {
    let style = text::TextStyle {
        font_family: font_family(config),
        size_px: config.font_size as f64,
        color: (0.95, 0.95, 1.0, 1.0),
        align_center: false,
    };
    text::layout_line(cr, inner, height, &style).2 <= width
}

/// Draw a `<glow color="#rrggbb">` soft halo behind a text span (built-in GPU
/// shader rendered via the cache and composited behind the text).
fn draw_glow(
    cr: &gtk::cairo::Context,
    rect: text::Rect,
    attrs: &[(String, String)],
    fx: &mut EffectCtx,
) {
    let (x, y, w, h) = rect;
    let pad = h * 0.6;
    let (gx, gy, gw, gh) = (x - pad, y - pad, w + 2.0 * pad, h + 2.0 * pad);

    let (r, g, b) = attrs
        .iter()
        .find(|(k, _)| k == "color")
        .and_then(|(_, v)| render::parse_hex_color(v))
        .map(|c| (c.r, c.g, c.b))
        .unwrap_or((0.40, 0.70, 1.0));

    let dw = (gw * fx.scale).round() as i32;
    let dh = (gh * fx.scale).round() as i32;
    if dw <= 0 || dh <= 0 {
        return;
    }
    let uniforms = [
        ("u_r".to_string(), r),
        ("u_g".to_string(), g),
        ("u_b".to_string(), b),
    ];
    if let Some(rgba) = fx.shaders.render(
        "builtin:glow",
        shader::GLOW_SRC,
        dw,
        dh,
        fx.time,
        fx.frame,
        &uniforms,
    ) {
        paint_rgba_at(cr, dw as usize, dh as usize, rgba, fx.scale, gx, gy);
    }
}

/// Draw a `<box bg="#rrggbb[aa]">` rounded highlight behind a text span.
fn draw_box(cr: &gtk::cairo::Context, rect: text::Rect, attrs: &[(String, String)]) {
    let (x, y, w, h) = rect;
    let pad = 4.0;
    let (rx, ry, rw, rh) = (x - pad, y - pad, w + 2.0 * pad, h + 2.0 * pad);

    let (r, g, b, a) = attrs
        .iter()
        .find(|(k, _)| k == "bg")
        .and_then(|(_, v)| render::parse_hex_color(v))
        .map(|c| (c.r as f64, c.g as f64, c.b as f64, c.a as f64))
        .unwrap_or((0.35, 0.45, 0.85, 0.55));

    rounded_rect(cr, rx, ry, rw, rh, rh * 0.32);
    cr.set_source_rgba(r, g, b, a);
    let _ = cr.fill();
}

/// Append a rounded-rectangle subpath to `cr`.
fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

#[inline]
fn glib_propagation_proceed() -> gtk::glib::Propagation {
    gtk::glib::Propagation::Proceed
}

waybar_module!(PwettyBox);

#[cfg(test)]
mod tests {
    use super::*;
    const P: char = markup::EMBED_PLACEHOLDER;

    #[test]
    fn hover_fades_between_states() {
        let wait = std::time::Duration::from_millis(HOVER_FADE_MS as u64 + 50);
        let h = Hover::new();
        assert_eq!(h.alpha(), 0.0);
        h.set(true);
        assert!(h.alpha() <= 1.0);
        std::thread::sleep(wait);
        assert_eq!(h.alpha(), 1.0);
        assert!(!h.animating());
        h.set(false);
        std::thread::sleep(wait);
        assert_eq!(h.alpha(), 0.0);
        // Content zoom rides the same fade: rest at 1.0, peak at HOVER_SCALE.
        assert_eq!(h.scale(), 1.0);
        h.set(true);
        std::thread::sleep(wait);
        assert_eq!(h.scale(), HOVER_SCALE);
    }

    #[test]
    fn flow_layout_single_line_with_embed() {
        let m = format!("a{P}b");
        assert_eq!(
            flow_layout(&m),
            vec![vec![
                FlowItem::Text("a"),
                FlowItem::Embed(0),
                FlowItem::Text("b")
            ]]
        );
    }

    #[test]
    fn flow_layout_multiline_indexes_embeds_in_document_order() {
        // line 0 has one embed; line 1 has two adjacent embeds then text.
        let m = format!("x{P}y\nz{P}{P}w");
        assert_eq!(
            flow_layout(&m),
            vec![
                vec![FlowItem::Text("x"), FlowItem::Embed(0), FlowItem::Text("y")],
                vec![
                    FlowItem::Text("z"),
                    FlowItem::Embed(1),
                    FlowItem::Embed(2),
                    FlowItem::Text("w"),
                ],
            ]
        );
    }

    #[test]
    fn flow_layout_leading_and_trailing_placeholders_emit_no_empty_text() {
        let m = format!("{P}mid{P}");
        assert_eq!(
            flow_layout(&m),
            vec![vec![
                FlowItem::Embed(0),
                FlowItem::Text("mid"),
                FlowItem::Embed(1)
            ]]
        );
    }

    #[test]
    fn flow_layout_plain_multiline_has_no_embeds() {
        assert_eq!(
            flow_layout("one\ntwo"),
            vec![vec![FlowItem::Text("one")], vec![FlowItem::Text("two")]]
        );
    }

    #[test]
    fn attr_finds_value_or_none() {
        let a = vec![("state".to_string(), "working".to_string())];
        assert_eq!(attr(&a, "state"), Some("working"));
        assert_eq!(attr(&a, "level"), None);
    }

    #[test]
    fn embed_width_uses_attr_or_default() {
        let none: Vec<(String, String)> = vec![];
        assert_eq!(embed_width(&none, 160.0), 160.0);
        let w = vec![("width".to_string(), "80".to_string())];
        assert_eq!(embed_width(&w, 160.0), 80.0);
    }

    #[test]
    fn osc_stays_within_range() {
        for &t in &[0.0_f32, 0.25, 0.5, 0.9, 1.7, 3.3] {
            let v = osc(t, 1.1, 0.18);
            assert!((0.18..=1.0).contains(&v), "osc out of range: {v}");
        }
    }

    #[test]
    fn idle_levels_cover_seven_steps() {
        assert_eq!(IDLE_LEVELS.len(), 7);
        assert_eq!(IDLE_LEVELS[0], "#ffffff");
        assert_eq!(IDLE_LEVELS[6], "#3a3a3a");
    }

    #[test]
    fn idle_age_color_keyframes() {
        let white = (1.0, 1.0, 1.0);
        let red = (1.0, 0.0, 0.0);
        let bright_purple = (0.78, 0.30, 1.0);
        let dark_purple = (0.30, 0.08, 0.45);

        // Full white below 5 min, regardless of unit.
        assert_eq!(idle_age_color("0s"), white);
        assert_eq!(idle_age_color("4m"), white);
        assert_eq!(idle_age_color("5m"), white);
        // Red from 1 h, held through 3 h.
        assert_eq!(idle_age_color("1h"), red);
        assert_eq!(idle_age_color("2h"), red);
        assert_eq!(idle_age_color("3h"), red);
        // Bright purple at 6 h, dark purple at 9 h and beyond.
        assert_eq!(idle_age_color("6h"), bright_purple);
        assert_eq!(idle_age_color("9h"), dark_purple);
        assert_eq!(idle_age_color("12h"), dark_purple);

        // Halfway between 3 h and 6 h: midway red→bright purple.
        let (r, g, b) = idle_age_color("270m"); // 4.5 h
        assert!((r - 0.89).abs() < 1e-6 && (g - 0.15).abs() < 1e-6 && (b - 0.5).abs() < 1e-6);
        // Within the 5..60 min window the red ramp climbs (green/blue drop).
        assert!(idle_age_color("12m").1 > idle_age_color("40m").1);
    }

    /// Render `markup` for the claude tile at `time`, direct or via `layer`.
    /// A tile-sized target as GTK hands one to a draw handler at integer
    /// `scale`: `scale`× the pixels, with a surface device scale so user space
    /// stays in logical px (fractional outputs like 1.25 render at scale 2).
    fn tile_surface(w: i32, h: i32, scale: i32) -> gtk::cairo::ImageSurface {
        use gtk::cairo::{Format, ImageSurface};
        let s = ImageSurface::create(Format::ARgb32, w * scale, h * scale).unwrap();
        s.set_device_scale(scale as f64, scale as f64);
        s
    }

    fn render_tile(markup: &str, time: f32, layer: Option<&mut Option<StaticLayer>>) -> Vec<u8> {
        render_tile_at(markup, time, layer, 1)
    }

    fn render_tile_at(
        markup: &str,
        time: f32,
        layer: Option<&mut Option<StaticLayer>>,
        scale: i32,
    ) -> Vec<u8> {
        use gtk::cairo::Context;
        let cfg = config::resolve(serde_json::json!({ "tile": "claude", "width": 179 }));
        let (w, h) = (cfg.width, cfg.height);
        let mut surface = tile_surface(w, h, scale);
        {
            let cr = Context::new(&surface).unwrap();
            let mut shaders = shader::ShaderCache::new();
            let mut fx = EffectCtx {
                shaders: &mut shaders,
                time,
                frame: 0,
                scale: scale as f64,
            };
            let (wf, hf) = (w as f64, h as f64);
            match layer {
                Some(l) => draw_content_cached(&cr, markup, wf, hf, &cfg, Some(&mut fx), l),
                None => draw_content(&cr, markup, wf, hf, &cfg, Some(&mut fx)),
            }
        }
        let px = surface.data().unwrap().to_vec();
        px
    }

    #[test]
    fn static_layer_matches_direct_draw_across_frames() {
        // A status glow overlaps the line below it: the cached layer must keep
        // it *under* that line (draw order), and the blink must still move.
        let cfg = config::resolve(serde_json::json!({ "tile": "claude" }));
        for sample in ["fleet", "working", "prompt", "idle"] {
            let path = format!(
                "{}/tiles/claude/samples/{sample}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            let data = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let markup = content::markup_for(&cfg, &data);
            // Scale 2 is what GTK uses on HiDPI and fractional (1.25) outputs.
            for scale in [1, 2] {
                let mut layer = None;
                for time in [0.0, 0.4, 1.1] {
                    let cached = render_tile_at(&markup, time, Some(&mut layer), scale);
                    let direct = render_tile_at(&markup, time, None, scale);
                    assert!(
                        cached == direct,
                        "{sample} @ t={time} scale {scale}: cached render differs from direct"
                    );
                }
                assert!(layer.is_some(), "{sample}: flow tile should be cached");
            }
        }
    }

    /// Bounding box `(x0, y0, x1, y1)` of pixels with visible alpha in a
    /// `w`-px-wide ARGB32 buffer.
    fn ink_box(px: &[u8], w: usize) -> (usize, usize, usize, usize) {
        let mut b = (usize::MAX, usize::MAX, 0, 0);
        for (i, p) in px.chunks_exact(4).enumerate() {
            if p[3] > 24 {
                let (x, y) = (i % w, i / w);
                b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
            }
        }
        b
    }

    #[test]
    fn hidpi_render_is_the_same_tile_twice_as_big() {
        // GTK draws a HiDPI (or 1.25-scaled) output at scale 2 via the surface's
        // device scale. Cached rasters once ignored it and drew text at half
        // size, up and left — while cached == direct still held, as both paths
        // shared the bug. So compare geometry against a scale-1 render instead.
        let cfg = config::resolve(serde_json::json!({ "tile": "claude" }));
        for sample in ["working", "fleet", "duo-window", "idle"] {
            let path = format!(
                "{}/tiles/claude/samples/{sample}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            let data = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let markup = content::markup_for(&cfg, &data);
            for cached in [false, true] {
                // A fresh layer per scale (a layer is keyed on its scale anyway).
                let render = |scale| {
                    let mut layer = None;
                    let l = cached.then_some(&mut layer);
                    render_tile_at(&markup, 0.4, l, scale)
                };
                let one = ink_box(&render(1), 179);
                let two = ink_box(&render(2), 179 * 2);
                // Halo/glow edges reach the tile's right edge on some samples
                // (clamped at both scales), so check the left, top and bottom
                // — the half-size bug moved all of them, by a lot.
                let near = |a: usize, b: usize| a.abs_diff(b) <= 4;
                assert!(
                    near(two.0, one.0 * 2) && near(two.1, one.1 * 2) && near(two.3, one.3 * 2 + 1),
                    "{sample} (cached: {cached}): scale-2 ink {two:?} isn't 2x scale-1 {one:?}"
                );
            }
        }
    }

    #[test]
    fn frame_damage_covers_everything_that_moves() {
        // What GTK does on an animation tick: keep last frame's pixels and
        // repaint only `frame_damage` (clipped). That must equal a full repaint
        // — anything a live op inks outside its bounds would go stale on screen.
        use gtk::cairo::{Context, Format, ImageSurface, Operator};
        let cfg = config::resolve(serde_json::json!({ "tile": "claude", "width": 179 }));
        let (w, h) = (cfg.width, cfg.height);
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tiles/claude/samples");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let data = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let markup = content::markup_for(&cfg, &data);
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            for (t0, t1) in [(0.0f32, 0.4f32), (0.4, 1.1), (1.1, 2.9)] {
                let mut layer = None;
                let mut shaders = shader::ShaderCache::new();
                let surface = ImageSurface::create(Format::ARgb32, w, h).unwrap();
                let draw = |cr: &Context, t, layer: &mut Option<StaticLayer>, sh: &mut _| {
                    let mut fx = EffectCtx {
                        shaders: sh,
                        time: t,
                        frame: 0,
                        scale: 1.0,
                    };
                    draw_content_cached(
                        cr,
                        &markup,
                        w as f64,
                        h as f64,
                        &cfg,
                        Some(&mut fx),
                        layer,
                    );
                };
                {
                    let cr = Context::new(&surface).unwrap();
                    draw(&cr, t0, &mut layer, &mut shaders);
                }
                let Some(rects) = layer.as_ref().and_then(StaticLayer::frame_damage) else {
                    continue; // whole-tile repaint: trivially correct
                };
                {
                    let cr = Context::new(&surface).unwrap();
                    for (x, y, rw, rh) in &rects {
                        let (x0, y0) = (x.floor(), y.floor());
                        cr.rectangle(x0, y0, (x + rw).ceil() - x0, (y + rh).ceil() - y0);
                    }
                    cr.clip();
                    // GTK clears the damaged area before the draw handler runs.
                    cr.set_operator(Operator::Clear);
                    cr.paint().unwrap();
                    cr.set_operator(Operator::Over);
                    draw(&cr, t1, &mut layer, &mut shaders);
                }
                let mut partial = surface;
                let partial = partial.data().unwrap().to_vec();
                let full = render_tile(&markup, t1, None);
                assert!(
                    partial == full,
                    "{name} {t0}->{t1}: pixels outside frame_damage changed"
                );
            }
        }
    }
}
