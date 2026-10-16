//! Tile-level background shader: a Shadertoy-style GLSL fragment shader rendered
//! on the GPU into an offscreen texture, read back as RGBA so the live widget can
//! composite it via Cairo (like the femtovg background layer).
//!
//! Requires a current GL context (the engine's [`crate::offscreen::OffscreenGl`]).

use std::collections::HashMap;

use glow::HasContext;

/// Bundled background-shader presets, selectable via `<bg preset="name"/>`. Each
/// is Shadertoy-style (`mainImage`), single-pass, texture-free. Compiled lazily
/// and masked to the focus bubble (see [`wrap_fragment_masked`]).
pub const PRESETS: &[(&str, &str)] = &[
    ("night", include_str!("../shaders/night.glsl")),
    ("caustic", include_str!("../shaders/caustic.glsl")),
];

/// GLSL source for a named background preset, if it exists.
pub fn preset(name: &str) -> Option<&'static str> {
    PRESETS.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// Built-in soft-glow shader (used by the `<glow color="…">` span effect). A
/// gently pulsing coloured blob with a soft elliptical falloff; the colour comes
/// from the `u_r`/`u_g`/`u_b` uniforms.
pub const GLOW_SRC: &str = "\
uniform float u_r; uniform float u_g; uniform float u_b;\n\
void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n\
    vec2 p = (fragCoord / iResolution.xy - 0.5) * 2.0;\n\
    float d = length(p * vec2(0.85, 1.0));\n\
    float a = smoothstep(1.0, 0.05, d) * 0.6 * (0.85 + 0.15 * sin(iTime * 2.5));\n\
    fragColor = vec4(u_r, u_g, u_b, clamp(a, 0.0, 1.0));\n\
}\n";

/// Compiles shaders on first use and caches them by key, so repeated frames (and
/// repeated span effects sharing a source) reuse one compiled program.
#[derive(Default)]
pub struct ShaderCache {
    passes: HashMap<String, Result<ShaderPass, String>>,
}

impl ShaderCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the shader under `key` (compiling `src` the first time) at `w`×`h`;
    /// returns RGBA8 (top-left), or `None` if it failed to compile.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        key: &str,
        src: &str,
        w: i32,
        h: i32,
        time: f32,
        frame: i32,
        uniforms: &[(String, f32)],
    ) -> Option<Vec<u8>> {
        self.render_inner(key, src, false, w, h, time, frame, uniforms)
    }

    /// Like [`render`](Self::render), but compiles with the focus-bubble mask
    /// wrapper ([`wrap_fragment_masked`]) — for `<bg>` preset backgrounds. The
    /// `u_bx/u_by/u_bw/u_bh/u_radius/u_fade/u_alpha` uniforms (passed by name in
    /// `uniforms`) drive the rounded-rect mask + overall opacity.
    #[allow(clippy::too_many_arguments)]
    pub fn render_masked(
        &mut self,
        key: &str,
        src: &str,
        w: i32,
        h: i32,
        time: f32,
        frame: i32,
        uniforms: &[(String, f32)],
    ) -> Option<Vec<u8>> {
        self.render_inner(key, src, true, w, h, time, frame, uniforms)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_inner(
        &mut self,
        key: &str,
        src: &str,
        masked: bool,
        w: i32,
        h: i32,
        time: f32,
        frame: i32,
        uniforms: &[(String, f32)],
    ) -> Option<Vec<u8>> {
        let entry = self.passes.entry(key.to_string()).or_insert_with(|| {
            let r = if masked {
                ShaderPass::new_masked(src)
            } else {
                ShaderPass::new(src)
            };
            if let Err(e) = &r {
                eprintln!("pwetty-box: shader '{key}' compile error:\n{e}");
            }
            r
        });
        entry
            .as_mut()
            .ok()
            .map(|pass| pass.render(w, h, time, frame, uniforms))
    }
}

/// Full-screen-triangle vertex shader (no vertex buffers needed).
const VERTEX_SRC: &str = "#version 300 es\n\
    void main() {\n\
        vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n\
        gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n\
    }\n";

/// Wrap Shadertoy-style user code (which defines
/// `void mainImage(out vec4 fragColor, in vec2 fragCoord)`) into a complete
/// GLES3 fragment shader exposing the standard `iResolution`/`iTime`/`iFrame`.
pub fn wrap_fragment(user: &str) -> String {
    format!(
        "#version 300 es\n\
         precision highp float;\n\
         uniform vec3 iResolution;\n\
         uniform float iTime;\n\
         uniform int iFrame;\n\
         out vec4 _pwetty_fragColor;\n\
         {user}\n\
         void main() {{ mainImage(_pwetty_fragColor, gl_FragCoord.xy); }}\n"
    )
}

/// As [`wrap_fragment`], but multiplies the shader's alpha by a rounded-rect
/// **focus-bubble mask** so `<bg>` presets read as a contained graphical layer.
/// The mask stacks a steep `u_fade`-px edge cliff (clean boundary) under a slow
/// `u_falloff`-px vignette (brightest deep inside, gently fading out). The preset
/// owns its *own* opacity via its `fragColor.a` (the wrapper-declared `u_alpha`
/// is available to it but no longer applied here — that lets a preset give, say,
/// its background and its highlights different alphas). Output is straight alpha
/// (premultiplied later by `paint_rgba_at`). Bubble bounds + radii are device px.
pub fn wrap_fragment_masked(user: &str) -> String {
    format!(
        "#version 300 es\n\
         precision highp float;\n\
         uniform vec3 iResolution;\n\
         uniform float iTime;\n\
         uniform int iFrame;\n\
         uniform float u_bx; uniform float u_by; uniform float u_bw; uniform float u_bh;\n\
         uniform float u_radius; uniform float u_fade; uniform float u_falloff; uniform float u_alpha;\n\
         out vec4 _pwetty_fragColor;\n\
         {user}\n\
         float _pw_sdbox(vec2 p, vec2 b, float r) {{\n\
             vec2 q = abs(p) - b + r;\n\
             return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;\n\
         }}\n\
         void main() {{\n\
             vec4 c; mainImage(c, gl_FragCoord.xy);\n\
             vec2 ctr = vec2(u_bx + u_bw * 0.5, u_by + u_bh * 0.5);\n\
             vec2 hlf = vec2(u_bw * 0.5, u_bh * 0.5);\n\
             float dist = -_pw_sdbox(gl_FragCoord.xy - ctr, hlf, u_radius);\n\
             // Two stacked fades, both on inside-distance from the bubble edge:\n\
             //   cliff = a steep cut over u_fade px right at the edge (clean boundary);\n\
             //   slow  = a gentle ramp over a much larger u_falloff px (a soft vignette,\n\
             //           brightest deep inside, dimming out toward the edge).\n\
             float cliff = clamp(dist / max(u_fade, 1.0), 0.0, 1.0);\n\
             float slow  = clamp(dist / max(u_falloff, 1.0), 0.0, 1.0);\n\
             _pwetty_fragColor = vec4(c.rgb, c.a * cliff * slow);\n\
         }}\n"
    )
}

/// A compiled background shader plus its offscreen render target.
pub struct ShaderPass {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    target: Option<(glow::Framebuffer, glow::Texture, i32, i32)>,
    /// One-shot log guard: target creation failure is reported once per pass,
    /// not per frame (the draw path runs at ~30fps).
    target_error_logged: bool,
}

impl ShaderPass {
    /// Compile `user_fragment` (Shadertoy `mainImage`). A GL context must be
    /// current. Returns the shader info log on compile/link failure.
    pub fn new(user_fragment: &str) -> Result<Self, String> {
        Self::compile(wrap_fragment(user_fragment))
    }

    /// Like [`new`](Self::new), but with the focus-bubble mask wrapper applied
    /// (see [`wrap_fragment_masked`]) — for `<bg>` preset backgrounds.
    pub fn new_masked(user_fragment: &str) -> Result<Self, String> {
        Self::compile(wrap_fragment_masked(user_fragment))
    }

    fn compile(fragment: String) -> Result<Self, String> {
        crate::gl::ensure_loaded();
        let gl = unsafe { glow::Context::from_loader_function(crate::gl::proc_addr) };
        unsafe {
            let program = link(&gl, VERTEX_SRC, &fragment)?;
            let vao = gl.create_vertex_array()?;
            Ok(Self {
                gl,
                program,
                vao,
                target: None,
                target_error_logged: false,
            })
        }
    }

    /// Render one frame at `w`×`h` device pixels and read it back as RGBA8,
    /// top-left origin (ready for [`crate::paint_rgba`]). `uniforms` are extra
    /// `float` uniforms (resolved from data) set by name; unknown names are
    /// ignored. If the render target can't be (re)created (context loss, GL
    /// object exhaustion), returns the zeroed (fully transparent) buffer so the
    /// caller composites nothing for this frame instead of panicking inside a
    /// GTK draw handler.
    pub fn render(
        &mut self,
        w: i32,
        h: i32,
        time: f32,
        frame: i32,
        uniforms: &[(String, f32)],
    ) -> Vec<u8> {
        let mut buf = vec![0u8; (w.max(1) * h.max(1) * 4) as usize];
        if !unsafe { self.ensure_target(w, h) } {
            if !self.target_error_logged {
                self.target_error_logged = true;
                eprintln!(
                    "pwetty-box: shader render-target creation failed (GL texture/framebuffer); \
                     skipping shader layer"
                );
            }
            return buf;
        }
        let gl = &self.gl;
        unsafe {
            let Some((fbo, ..)) = self.target else {
                return buf;
            };
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, w, h);
            // Deterministic state: another renderer (femtovg) may have left
            // blending on with garbage in this FBO; we want the shader output to
            // fully replace the target.
            gl.disable(glow::BLEND);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.use_program(Some(self.program));
            if let Some(loc) = gl.get_uniform_location(self.program, "iResolution") {
                gl.uniform_3_f32(Some(&loc), w as f32, h as f32, 1.0);
            }
            if let Some(loc) = gl.get_uniform_location(self.program, "iTime") {
                gl.uniform_1_f32(Some(&loc), time);
            }
            if let Some(loc) = gl.get_uniform_location(self.program, "iFrame") {
                gl.uniform_1_i32(Some(&loc), frame);
            }
            // Data-driven uniforms (ignored if the shader doesn't declare them).
            for (name, value) in uniforms {
                if let Some(loc) = gl.get_uniform_location(self.program, name) {
                    gl.uniform_1_f32(Some(&loc), *value);
                }
            }
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.read_pixels(
                0,
                0,
                w,
                h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
        }
        flip_rows(&mut buf, w.max(1) as usize, h.max(1) as usize);
        buf
    }

    /// (Re)create the color texture + FBO when the size changes. Returns
    /// `false` (with `self.target` left empty) if GL object creation fails —
    /// the caller degrades to a transparent frame rather than panicking.
    unsafe fn ensure_target(&mut self, w: i32, h: i32) -> bool {
        if self.target.map(|(_, _, tw, th)| (tw, th)) == Some((w, h)) {
            return true;
        }
        let gl = &self.gl;
        if let Some((fbo, tex, _, _)) = self.target.take() {
            gl.delete_framebuffer(fbo);
            gl.delete_texture(tex);
        }
        let Ok(tex) = gl.create_texture() else {
            return false;
        };
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        let Ok(fbo) = gl.create_framebuffer() else {
            gl.delete_texture(tex);
            return false;
        };
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        self.target = Some((fbo, tex, w, h));
        true
    }
}

unsafe fn link(gl: &glow::Context, vs: &str, fs: &str) -> Result<glow::Program, String> {
    let v = compile(gl, glow::VERTEX_SHADER, vs)?;
    let f = compile(gl, glow::FRAGMENT_SHADER, fs)?;
    let program = gl.create_program()?;
    gl.attach_shader(program, v);
    gl.attach_shader(program, f);
    gl.link_program(program);
    let ok = gl.get_program_link_status(program);
    gl.delete_shader(v);
    gl.delete_shader(f);
    if ok {
        Ok(program)
    } else {
        Err(gl.get_program_info_log(program))
    }
}

unsafe fn compile(gl: &glow::Context, kind: u32, src: &str) -> Result<glow::Shader, String> {
    let shader = gl.create_shader(kind)?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if gl.get_shader_compile_status(shader) {
        Ok(shader)
    } else {
        Err(gl.get_shader_info_log(shader))
    }
}

/// Flip image rows in place (GL's bottom-left origin → top-left for Cairo).
fn flip_rows(buf: &mut [u8], w: usize, h: usize) {
    let stride = w * 4;
    for y in 0..h / 2 {
        let (top, bot) = (y * stride, (h - 1 - y) * stride);
        for i in 0..stride {
            buf.swap(top + i, bot + i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{preset, wrap_fragment, wrap_fragment_masked, PRESETS};

    #[test]
    fn wrap_adds_boilerplate_and_user_code() {
        let s = wrap_fragment("void mainImage(out vec4 c, in vec2 p){ c = vec4(1.0); }");
        assert!(s.starts_with("#version 300 es"));
        assert!(s.contains("uniform float iTime;"));
        assert!(s.contains("uniform vec3 iResolution;"));
        assert!(s.contains("void mainImage")); // user code present
        assert!(s.contains("mainImage(_pwetty_fragColor, gl_FragCoord.xy)"));
    }

    #[test]
    fn masked_wrap_adds_bubble_uniforms_and_mask() {
        let s = wrap_fragment_masked("void mainImage(out vec4 c, in vec2 p){ c = vec4(1.0); }");
        assert!(s.starts_with("#version 300 es"));
        // bubble + alpha uniforms the renderer drives by name
        for u in [
            "u_bx",
            "u_by",
            "u_bw",
            "u_bh",
            "u_radius",
            "u_fade",
            "u_falloff",
            "u_alpha",
        ] {
            assert!(s.contains(u), "missing uniform {u}");
        }
        assert!(s.contains("_pw_sdbox")); // rounded-rect mask
                                          // wrapper applies only the mask (cliff * slow); the preset owns u_alpha
        assert!(s.contains("c.a * cliff * slow"));
    }

    #[test]
    fn presets_resolve_by_name() {
        assert!(preset("night").is_some());
        assert!(preset("caustic").is_some());
        assert!(preset("nope").is_none());
        // every bundled preset defines a mainImage entry point
        for (name, src) in PRESETS {
            assert!(src.contains("mainImage"), "{name} has no mainImage");
        }
    }
}
