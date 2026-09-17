// WebGL2 glyph atlas terminal renderer
//
// Two-pass instanced rendering:
//   Pass 0 (mode=0): background rects — one full-cell colored quad per cell
//   Pass 1 (mode=1): text — glyph quads (atlas alpha) plus flat geometry rects
//                    for box-drawing/block cells (solid atlas texel)
//
// The two passes read from separate instance buffers (bg vs text) because a
// graphic cell draws a full-cell background but a sub-rect text glyph.
//
// Per-instance data: 16 floats
//   [0..2]  offset (px)
//   [2..4]  size (px)
//   [4..8]  UV (atlas coordinates)
//   [8..11] fg_rgb (foreground color)
//   [11..14] bg_rgb (background color)
//   [14]    selection flag (1.0 = selected, 0.0 = not)
//   [15]    cursor flag (1.0 = cursor cell, 0.0 = not)
//
// Color overrides: resolved by the shared `crate::color::cell_visual` helper
// (same decision as the Canvas 2D path):
//   selected: fg = black, bg = original fg (highlight)
//   cursor:   fg = original bg, bg = original fg (block cursor)

use crate::color::{cell_visual, CellOverride};
use crate::ffi::{self, JsHandle};
use crate::graphics::graphic_cell_rects;
#[cfg(target_arch = "wasm32")]
use crate::measure::FONT_FAMILIES;
use crate::measure::FONT_SIZE_CSS;

// --- WebGL2 constants (the "krust" JS runtime mirrors the browser values) ---
const VERTEX_SHADER_KIND: u32 = 0x8B31;
const FRAGMENT_SHADER_KIND: u32 = 0x8B30;
const COMPILE_STATUS: u32 = 0x8B81;
const LINK_STATUS: u32 = 0x8B82;
const ARRAY_BUFFER: u32 = 0x8892;
const STATIC_DRAW: u32 = 0x88E4;
const DYNAMIC_DRAW: u32 = 0x88E8;
const FLOAT: u32 = 0x1406;
const TEXTURE0: u32 = 0x84C0;
const TEXTURE_2D: u32 = 0x0DE1;
const TEXTURE_MIN_FILTER: u32 = 0x2801;
const TEXTURE_MAG_FILTER: u32 = 0x2800;
const TEXTURE_WRAP_S: u32 = 0x2802;
const TEXTURE_WRAP_T: u32 = 0x2803;
const NEAREST: i32 = 0x2600;
const CLAMP_TO_EDGE: i32 = 0x812F;
const ALPHA: u32 = 0x1906;
const UNSIGNED_BYTE: u32 = 0x1401;
const COLOR_BUFFER_BIT: u32 = 0x4000;
const TRIANGLE_STRIP: u32 = 0x0005;

const ATLAS_PADDING: u32 = 2;
const ATLAS_COLS: usize = 32;

/// A contiguous range of codepoints baked into the atlas.
struct AtlasRange {
    start: u32,
    len: u32,
    offset: u32, // index into GlyphAtlas.uv_map
}

/// Ranges baked into the glyph atlas. Kept sorted by `start` so
/// `uv_for()` can scan linearly (the list is tiny).
const ATLAS_RANGES: &[AtlasRange] = &[
    AtlasRange { start: 0x0000, len: 128, offset: 0 },    // ASCII
    AtlasRange { start: 0x2190, len: 112, offset: 128 },  // Arrows (spinner ↻ ↺ ← →)
    AtlasRange { start: 0x2200, len: 256, offset: 240 },  // Math operators (⋯ ⊶ ⊷ ⇦ ⇨)
    AtlasRange { start: 0x2580, len: 128, offset: 496 },  // Block parts (U+2581-259F) + Geometric shapes (U+25A0-25FF)
    AtlasRange { start: 0x2700, len: 192, offset: 624 },  // Dingbats (✦ ✧ ✶ ✔)
    AtlasRange { start: 0x2800, len: 256, offset: 816 },  // Braille
    AtlasRange { start: 0x00A0, len: 96, offset: 1072 },  // Latin-1 Supplement (· ° ± « »)
    AtlasRange { start: 0x2000, len: 112, offset: 1168 }, // General Punctuation (… – — ‘ ’ “ ”)
    AtlasRange { start: 0x20A0, len: 48, offset: 1280 },  // Currency (€ £ ¥)
    AtlasRange { start: 0x2600, len: 256, offset: 1328 }, // Misc Symbols (☀ ⚙ ⚠ ★)
];

/// Per-instance floats: offset(2) + size(2) + uv(4) + fg(3) + bg(3) + sel(1) + cur(1) = 16
const INSTANCE_FLOATS: usize = 16;
/// Byte stride = 16 * 4
const INSTANCE_STRIDE: i32 = (INSTANCE_FLOATS * 4) as i32;

const VERTEX_SHADER: &str = r#"
attribute vec2 a_position;
attribute vec2 a_texcoord;
attribute vec2 a_offset;
attribute vec2 a_size;
attribute vec4 a_uv;
attribute vec3 a_fg;
attribute vec3 a_bg;
attribute float a_sel;
attribute float a_cur;
varying vec2 v_texcoord;
varying vec3 v_fg;
varying vec3 v_bg;
varying float v_sel;
varying float v_cur;
uniform vec2 u_resolution;
void main() {
    vec2 scaled = a_position * a_size + a_offset;
    // Pixel y=0 is the canvas TOP, the same convention as the Canvas 2D
    // renderer. The raw NDC y is bottom-up, so negate it to flip the axis.
    // a_texcoord's v is flipped too so the atlas's stored glyph orientation
    // (top row at zw) still lands at the quad's top edge.
    gl_Position = vec4(scaled.x / u_resolution.x * 2.0 - 1.0,
                       -(scaled.y / u_resolution.y * 2.0 - 1.0),
                       0.0, 1.0);
    v_texcoord = mix(a_uv.xy, a_uv.zw, vec2(a_texcoord.x, 1.0 - a_texcoord.y));
    v_fg = a_fg;
    v_bg = a_bg;
    v_sel = a_sel;
    v_cur = a_cur;
}
"#;

const FRAGMENT_SHADER: &str = r#"
precision mediump float;
varying vec2 v_texcoord;
varying vec3 v_fg;
varying vec3 v_bg;
varying float v_sel;
varying float v_cur;
uniform sampler2D u_atlas;
uniform float u_mode;
void main() {
    if (u_mode < 0.5) {
        // Background pass: output per-cell background color
        gl_FragColor = vec4(v_bg, 1.0);
    } else {
        // Text pass: blend foreground over background using atlas alpha
        // Colors in v_fg/v_bg are already swapped for selection/cursor by build_instances
        float alpha = texture2D(u_atlas, v_texcoord).a;
        vec3 color = mix(v_bg, v_fg, alpha);
        gl_FragColor = vec4(color, 1.0);
    }
}
"#;

pub struct GlyphAtlas {
    pub texture: JsHandle,
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub glyph_width: u32,
    pub glyph_height: u32,
    pub uv_map: Vec<(f32, f32, f32, f32)>,
}

impl GlyphAtlas {
    pub fn new(gl: JsHandle, cell_w: f64, cell_h: f64, dpr: f64) -> Result<Self, String> {
        let glyph_w = (cell_w * dpr).ceil() as u32;
        let glyph_h = (cell_h * dpr).ceil() as u32;

        // Count total glyphs across all ranges.
        let total: u32 = ATLAS_RANGES.iter().map(|r| r.len).sum();
        let cols = ATLAS_COLS as u32;
        let rows = (total + cols - 1) / cols;
        let atlas_w = cols * (glyph_w + ATLAS_PADDING);
        let atlas_h = rows * (glyph_h + ATLAS_PADDING);

        let mut data: Vec<u8> = vec![0; (atlas_w * atlas_h) as usize];
        let mut uv_map: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(total as usize);

        for range in ATLAS_RANGES {
            for i in 0..range.len {
                let idx = range.offset + i;
                let col = idx % cols;
                let row = idx / cols;
                let x = col * (glyph_w + ATLAS_PADDING);
                let y = row * (glyph_h + ATLAS_PADDING);

                let u0 = x as f32 / atlas_w as f32;
                let v0 = y as f32 / atlas_h as f32;
                let u1 = (x + glyph_w) as f32 / atlas_w as f32;
                let v1 = (y + glyph_h) as f32 / atlas_h as f32;
                uv_map.push((u0, v1, u1, v0));
            }
        }

        // Rasterize the glyphs with the browser's Canvas 2D text engine. This
        // is the *same* engine, font stack and font size the Canvas 2D renderer
        // paints with, so the GL atlas matches the reference path exactly and
        // the browser's per-glyph fallback resolves glyphs no single family has.
        Self::rasterize_atlas(&mut data, atlas_w, glyph_w, glyph_h, FONT_SIZE_CSS * dpr);

        // Braille (U+2800..U+28FF) is synthesized as a 2x4 dot grid instead of
        // font-rendered: at terminal sizes spinner frames like ⠋/⠙ can collapse
        // to identical bitmaps in a font, making opencode's spinner look frozen
        // on the GL path. Overwrite those slots after the font pass.
        for range in ATLAS_RANGES {
            if range.start != 0x2800 {
                continue;
            }
            for i in 0..range.len {
                let idx = range.offset + i;
                let col = idx % cols;
                let row = idx / cols;
                let x = col * (glyph_w + ATLAS_PADDING);
                let y = row * (glyph_h + ATLAS_PADDING);
                Self::rasterize_braille(
                    range.start + i,
                    glyph_w,
                    glyph_h,
                    &mut data,
                    x,
                    y,
                    atlas_w,
                );
            }
        }

        // Reserve the bottom-right padding texel as an opaque "solid" sample:
        // graphic cells (box drawing / block elements) point their UVs here so
        // the text pass paints a flat foreground color instead of a glyph.
        data[((atlas_h - 1) * atlas_w + (atlas_w - 1)) as usize] = 255;

        let texture = Self::upload_texture(gl, &data, atlas_w, atlas_h)?;

        Ok(GlyphAtlas {
            texture,
            atlas_width: atlas_w,
            atlas_height: atlas_h,
            glyph_width: glyph_w,
            glyph_height: glyph_h,
            uv_map,
        })
    }

    /// Bake every atlas slot with the browser's Canvas 2D `fillText`, using the
    /// shared [`FONT_FAMILIES`] stack and `textBaseline = "middle"` at the
    /// cell's vertical centre — exactly how `TerminalState::paint_cell` draws
    /// text — then copy each slot's alpha into the atlas bitmap.
    ///
    /// No-op without a DOM (host tests): the atlas stays blank and the
    /// font-independent logic (UVs, braille, geometry) is what tests exercise.
    #[cfg(target_arch = "wasm32")]
    fn rasterize_atlas(
        data: &mut [u8],
        atlas_w: u32,
        glyph_w: u32,
        glyph_h: u32,
        font_px: f64,
    ) {
        let win = ffi::window();
        if win == 0 {
            return;
        }
        let doc = ffi::window_document(win);
        if doc == 0 {
            return;
        }
        let canvas = ffi::document_create_canvas(doc);
        if canvas == 0 {
            return;
        }

        let cols = ATLAS_COLS as u32;
        let total: u32 = ATLAS_RANGES.iter().map(|r| r.len).sum();
        let rows = (total + cols - 1) / cols;
        // Double-pitch each isolated cell so a wide glyph (or an overhanging
        // one) cannot bleed into a neighbour before the single `getImageData`.
        let pitch_x = glyph_w * 2;
        let pitch_y = glyph_h * 2;
        let cw = cols * pitch_x;
        let chh = rows * pitch_y;

        ffi::canvas_set_width(canvas, cw);
        ffi::canvas_set_height(canvas, chh);
        let ctx = ffi::canvas_get_2d(canvas);
        if ctx == 0 {
            ffi::release(canvas);
            return;
        }
        ffi::ctx_set_font(ctx, &format!("{}px {}", font_px, FONT_FAMILIES));
        ffi::ctx_set_fill_style(ctx, "#ffffff");
        ffi::ctx_set_text_baseline(ctx, "middle");

        for range in ATLAS_RANGES {
            for i in 0..range.len {
                let cp = range.start + i;
                // Control codes never paint; braille is synthesized separately.
                if cp < 0x20 || cp == 0x7F || (0x2800..=0x28FF).contains(&cp) {
                    continue;
                }
                let Some(ch) = char::from_u32(cp) else {
                    continue;
                };
                let idx = range.offset + i;
                let col = idx % cols;
                let row = idx / cols;
                ffi::ctx_fill_text(
                    ctx,
                    &ch.to_string(),
                    (col * pitch_x) as f64,
                    (row * pitch_y) as f64 + glyph_h as f64 * 0.5,
                );
            }
        }

        let mut px = vec![0u8; (cw * chh * 4) as usize];
        let written = ffi::ctx_get_image_data(ctx, 0.0, 0.0, cw as f64, chh as f64, &mut px);
        ffi::release(ctx);
        ffi::release(canvas);
        if written < px.len() {
            return;
        }

        for range in ATLAS_RANGES {
            for i in 0..range.len {
                let idx = range.offset + i;
                let slot_col = idx % cols;
                let slot_row = idx / cols;
                let sx = slot_col * (glyph_w + ATLAS_PADDING);
                let sy = slot_row * (glyph_h + ATLAS_PADDING);
                for gy in 0..glyph_h {
                    for gx in 0..glyph_w {
                        let a = px[(((slot_row * pitch_y + gy) * cw)
                            + slot_col * pitch_x
                            + gx) as usize
                            * 4
                            + 3];
                        if a > 0 {
                            data[((sy + gy) * atlas_w + sx + gx) as usize] = a;
                        }
                    }
                }
            }
        }
    }

    /// Native (host-test) stub: there is no DOM to rasterize with, so the atlas
    /// stays blank. The browser-facing FFI imports are never linked here.
    #[cfg(not(target_arch = "wasm32"))]
    fn rasterize_atlas(
        _data: &mut [u8],
        _atlas_w: u32,
        _glyph_w: u32,
        _glyph_h: u32,
        _font_px: f64,
    ) {
    }

    /// Paint a braille pattern (U+2800..U+28FF) as a 2x4 grid of filled dots
    /// into the atlas slot at `(x, y)`. Braille bit layout (dot N = bit N-1):
    ///   dot1 dot4
    ///   dot2 dot5
    ///   dot3 dot6
    ///   dot7 dot8
    fn rasterize_braille(
        cp: u32,
        glyph_w: u32,
        glyph_h: u32,
        data: &mut [u8],
        x: u32,
        y: u32,
        stride: u32,
    ) {
        // Two dots per row across the width; four rows stacked over the height.
        let xs = [glyph_w as f32 * 0.3125, glyph_w as f32 * 0.6875];
        let ys = [
            glyph_h as f32 * 0.17,
            glyph_h as f32 * 0.42,
            glyph_h as f32 * 0.67,
            glyph_h as f32 * 0.92,
        ];
        let cx = [
            x + xs[0].round() as u32,
            x + xs[1].round() as u32,
        ];
        let cy = [
            y + ys[0].round() as u32,
            y + ys[1].round() as u32,
            y + ys[2].round() as u32,
            y + ys[3].round() as u32,
        ];
        // Dot diameter ~2px at an 8px cell, scaled up with the slot.
        let r = ((glyph_w as f32 * 0.19).floor() as u32).max(1);
        for dot in 0..8u32 {
            if cp & (1u32 << dot) == 0 {
                continue;
            }
            let (row, col) = match dot {
                0 => (0, 0),
                1 => (1, 0),
                2 => (2, 0),
                3 => (0, 1),
                4 => (1, 1),
                5 => (2, 1),
                6 => (3, 0),
                7 => (3, 1),
                _ => unreachable!(),
            };
            let dcx = cx[col];
            let dcy = cy[row];
            let x0 = dcx.saturating_sub(r).max(x);
            let x1 = (dcx + r).min(x + glyph_w - 1);
            let y0 = dcy.saturating_sub(r).max(y);
            let y1 = (dcy + r).min(y + glyph_h - 1);
            for py in y0..=y1 {
                for px in x0..=x1 {
                    data[(py * stride + px) as usize] = 255;
                }
            }
        }
    }

    fn upload_texture(
        gl: JsHandle,
        data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<JsHandle, String> {
        let texture = ffi::gl_create_texture(gl);
        if texture == 0 {
            return Err("create_texture".to_string());
        }
        ffi::gl_bind_texture(gl, TEXTURE_2D, texture);
        ffi::gl_tex_parameteri(gl, TEXTURE_2D, TEXTURE_MIN_FILTER, NEAREST);
        ffi::gl_tex_parameteri(gl, TEXTURE_2D, TEXTURE_MAG_FILTER, NEAREST);
        ffi::gl_tex_parameteri(gl, TEXTURE_2D, TEXTURE_WRAP_S, CLAMP_TO_EDGE);
        ffi::gl_tex_parameteri(gl, TEXTURE_2D, TEXTURE_WRAP_T, CLAMP_TO_EDGE);

        ffi::gl_tex_image_2d_alpha(
            gl,
            TEXTURE_2D,
            0,
            ALPHA as i32,
            width as i32,
            height as i32,
            0,
            ALPHA,
            UNSIGNED_BYTE,
            data,
        );
        Ok(texture)
    }

    pub fn uv_for(&self, ch: char) -> Option<(f32, f32, f32, f32)> {
        let cp = ch as u32;
        for range in ATLAS_RANGES {
            if cp >= range.start && cp < range.start + range.len {
                let idx = (range.offset + (cp - range.start)) as usize;
                return self.uv_map.get(idx).copied();
            }
        }
        None
    }

    /// UV quad centered on the reserved opaque texel, for flat-color fills.
    pub fn solid_uv(&self) -> (f32, f32, f32, f32) {
        let u = (self.atlas_width as f32 - 0.5) / self.atlas_width as f32;
        let v = (self.atlas_height as f32 - 0.5) / self.atlas_height as f32;
        (u, v, u, v)
    }

    pub fn rebuild(
        &mut self,
        gl: JsHandle,
        cell_w: f64,
        cell_h: f64,
        dpr: f64,
    ) -> Result<(), String> {
        let new_atlas = Self::new(gl, cell_w, cell_h, dpr)?;
        ffi::gl_delete_texture(gl, self.texture);
        self.texture = new_atlas.texture;
        self.atlas_width = new_atlas.atlas_width;
        self.atlas_height = new_atlas.atlas_height;
        self.glyph_width = new_atlas.glyph_width;
        self.glyph_height = new_atlas.glyph_height;
        self.uv_map = new_atlas.uv_map;
        Ok(())
    }
}

pub struct GlyphBrush {
    pub program: JsHandle,
    pub pos_buffer: JsHandle,
    pub uv_buffer: JsHandle,
    pub bg_instance_buffer: JsHandle,
    pub text_instance_buffer: JsHandle,
    pub resolution_loc: JsHandle,
    pub atlas_loc: JsHandle,
    pub mode_loc: JsHandle,
}

impl GlyphBrush {
    pub fn new(gl: JsHandle) -> Result<Self, String> {
        let program = Self::compile_program(gl)?;

        let pos_buffer = Self::new_buffer(gl, "pos_buffer")?;
        let uv_buffer = Self::new_buffer(gl, "uv_buffer")?;
        let bg_instance_buffer = Self::new_buffer(gl, "bg_instance_buffer")?;
        let text_instance_buffer = Self::new_buffer(gl, "text_instance_buffer")?;

        let quad_verts: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
        let quad_uvs: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];

        ffi::gl_bind_buffer(gl, ARRAY_BUFFER, pos_buffer);
        ffi::gl_buffer_data_f32(gl, ARRAY_BUFFER, &quad_verts, STATIC_DRAW);

        ffi::gl_bind_buffer(gl, ARRAY_BUFFER, uv_buffer);
        ffi::gl_buffer_data_f32(gl, ARRAY_BUFFER, &quad_uvs, STATIC_DRAW);

        let resolution_loc = Self::new_uniform(gl, program, "u_resolution")?;
        let atlas_loc = Self::new_uniform(gl, program, "u_atlas")?;
        let mode_loc = Self::new_uniform(gl, program, "u_mode")?;

        Ok(GlyphBrush {
            program,
            pos_buffer,
            uv_buffer,
            bg_instance_buffer,
            text_instance_buffer,
            resolution_loc,
            atlas_loc,
            mode_loc,
        })
    }

    fn new_buffer(gl: JsHandle, name: &str) -> Result<JsHandle, String> {
        let buf = ffi::gl_create_buffer(gl);
        if buf == 0 {
            return Err(name.to_string());
        }
        Ok(buf)
    }

    fn new_uniform(gl: JsHandle, program: JsHandle, name: &str) -> Result<JsHandle, String> {
        let loc = ffi::gl_get_uniform_location(gl, program, name);
        if loc == 0 {
            return Err(name.to_string());
        }
        Ok(loc)
    }

    fn compile_program(gl: JsHandle) -> Result<JsHandle, String> {
        let vs = Self::compile_shader(gl, VERTEX_SHADER_KIND, VERTEX_SHADER)?;
        let fs = Self::compile_shader(gl, FRAGMENT_SHADER_KIND, FRAGMENT_SHADER)?;
        let program = ffi::gl_create_program(gl);
        if program == 0 {
            return Err("create_program".to_string());
        }
        ffi::gl_attach_shader(gl, program, vs);
        ffi::gl_attach_shader(gl, program, fs);
        ffi::gl_link_program(gl, program);
        if ffi::gl_get_program_parameter(gl, program, LINK_STATUS) == 0 {
            let mut log = vec![0u8; 4096];
            let n = ffi::gl_get_program_info_log(gl, program, &mut log);
            let log = String::from_utf8_lossy(&log[..n]);
            return Err(format!("link_program failed: {}", log));
        }
        Ok(program)
    }

    fn compile_shader(gl: JsHandle, kind: u32, source: &str) -> Result<JsHandle, String> {
        let shader = ffi::gl_create_shader(gl, kind);
        if shader == 0 {
            return Err("create_shader".to_string());
        }
        ffi::gl_shader_source(gl, shader, source);
        ffi::gl_compile_shader(gl, shader);
        if ffi::gl_get_shader_parameter(gl, shader, COMPILE_STATUS) == 0 {
            let mut log = vec![0u8; 4096];
            let n = ffi::gl_get_shader_info_log(gl, shader, &mut log);
            let log = String::from_utf8_lossy(&log[..n]);
            return Err(format!("compile_shader failed: {}", log));
        }
        Ok(shader)
    }
}

/// Decompose a packed RGB u32 into (r, g, b) unit floats.
fn rgb_to_floats(rgb: u32) -> (f32, f32, f32) {
    (
        ((rgb >> 16) & 0xff) as f32 / 255.0,
        ((rgb >> 8) & 0xff) as f32 / 255.0,
        (rgb & 0xff) as f32 / 255.0,
    )
}

pub struct WebGL2Renderer {
    /// Canvas element backing the GL context, queried for drawing-buffer size.
    pub canvas: JsHandle,
    pub ctx: JsHandle,
    pub atlas: GlyphAtlas,
    pub brush: GlyphBrush,
    pub cell_w: u32,
    pub cell_h: u32,
    pub rows: u16,
    pub cols: u16,
    pub dpr: f64,
}

impl WebGL2Renderer {
    pub fn new(
        canvas_id: &str,
        cell_w: f64,
        cell_h: f64,
        rows: u16,
        cols: u16,
        dpr: f64,
    ) -> Result<Self, String> {
        let win = ffi::window();
        if win == 0 {
            return Err("window unavailable".to_string());
        }
        let doc = ffi::window_document(win);
        if doc == 0 {
            return Err("document unavailable".to_string());
        }
        let canvas = ffi::document_get_element_by_id(doc, canvas_id);
        if canvas == 0 {
            return Err(format!("canvas '#{}' not found", canvas_id));
        }

        let gl = ffi::canvas_get_webgl2(canvas);
        if gl == 0 {
            return Err("webgl2 context unavailable".to_string());
        }

        let atlas = GlyphAtlas::new(gl, cell_w, cell_h, dpr)?;
        let brush = GlyphBrush::new(gl)?;

        Ok(WebGL2Renderer {
            canvas,
            ctx: gl,
            atlas,
            brush,
            cell_w: (cell_w * dpr).ceil() as u32,
            cell_h: (cell_h * dpr).ceil() as u32,
            rows,
            cols,
            dpr,
        })
    }

    pub fn rebuild_atlas(&mut self) -> Result<(), String> {
        let css_w = self.cell_w as f64 / self.dpr;
        let css_h = self.cell_h as f64 / self.dpr;
        self.atlas.rebuild(self.ctx, css_w, css_h, self.dpr)
    }

    pub fn render(
        &self,
        screen: &vt100::Screen,
        default_fg: u32,
        default_bg: u32,
        selection: &[(u16, u16)],
        cursor: (u16, u16),
    ) -> Result<(), String> {
        let (prows, pcols) = screen.size();
        let rrows = if self.rows > 0 { self.rows } else { prows as u16 };
        let rcols = if self.cols > 0 { self.cols } else { pcols as u16 };
        let rows = rrows as u32;
        let cols = rcols as u32;

        let gl = self.ctx;
        // Use the full drawing buffer as the viewport so the destination rect
        // always matches it (avoids Firefox's "Drawing to a destination rect
        // smaller than the viewport rect" warning). The grid is laid out from
        // pixel origin (0,0) downward (top-left convention), so it occupies
        // the top-left corner of the buffer.
        let buf_w = ffi::canvas_width(self.canvas);
        let buf_h = ffi::canvas_height(self.canvas);

        ffi::gl_viewport(gl, 0, 0, buf_w as i32, buf_h as i32);
        let (cr, cg, cb) = rgb_to_floats(default_bg);
        ffi::gl_clear_color(gl, cr, cg, cb, 1.0);
        ffi::gl_clear(gl, COLOR_BUFFER_BIT);

        ffi::gl_use_program(gl, self.brush.program);
        ffi::gl_uniform2f(gl, self.brush.resolution_loc, buf_w as f32, buf_h as f32);

        ffi::gl_active_texture(gl, TEXTURE0);
        ffi::gl_bind_texture(gl, TEXTURE_2D, self.atlas.texture);
        ffi::gl_uniform1i(gl, self.brush.atlas_loc, 0);

        let (bg_instances, text_instances) = Self::build_instances(
            rows, cols, self.cell_w, self.cell_h, &self.atlas, screen,
            prows, pcols, selection, cursor, default_fg, default_bg,
            self.dpr,
        );
        let bg_count = bg_instances.len() / INSTANCE_FLOATS;
        let text_count = text_instances.len() / INSTANCE_FLOATS;

        let pos_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_position") as u32;
        let tex_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_texcoord") as u32;
        let off_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_offset") as u32;
        let size_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_size") as u32;
        let uv_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_uv") as u32;
        let fg_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_fg") as u32;
        let bg_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_bg") as u32;
        let sel_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_sel") as u32;
        let cur_attr = ffi::gl_get_attrib_location(gl, self.brush.program, "a_cur") as u32;

        // Per-vertex attributes (shared across all instances)
        ffi::gl_bind_buffer(gl, ARRAY_BUFFER, self.brush.pos_buffer);
        ffi::gl_enable_vertex_attrib_array(gl, pos_attr);
        ffi::gl_vertex_attrib_pointer(gl, pos_attr, 2, FLOAT, false, 0, 0);

        ffi::gl_bind_buffer(gl, ARRAY_BUFFER, self.brush.uv_buffer);
        ffi::gl_enable_vertex_attrib_array(gl, tex_attr);
        ffi::gl_vertex_attrib_pointer(gl, tex_attr, 2, FLOAT, false, 0, 0);

        // Points the per-instance attributes at `buffer`. Called once per pass;
        // attribute pointers are stored per-attribute at pointer-set time, so
        // re-pointing with a different buffer bound switches the source.
        let bind_instances = |gl: JsHandle, buffer: JsHandle| {
            ffi::gl_bind_buffer(gl, ARRAY_BUFFER, buffer);

            ffi::gl_enable_vertex_attrib_array(gl, off_attr);
            ffi::gl_vertex_attrib_pointer(gl, off_attr, 2, FLOAT, false, INSTANCE_STRIDE, 0);
            ffi::gl_vertex_attrib_divisor(gl, off_attr, 1);

            ffi::gl_enable_vertex_attrib_array(gl, size_attr);
            ffi::gl_vertex_attrib_pointer(gl, size_attr, 2, FLOAT, false, INSTANCE_STRIDE, 8);
            ffi::gl_vertex_attrib_divisor(gl, size_attr, 1);

            ffi::gl_enable_vertex_attrib_array(gl, uv_attr);
            ffi::gl_vertex_attrib_pointer(gl, uv_attr, 4, FLOAT, false, INSTANCE_STRIDE, 16);
            ffi::gl_vertex_attrib_divisor(gl, uv_attr, 1);

            ffi::gl_enable_vertex_attrib_array(gl, fg_attr);
            ffi::gl_vertex_attrib_pointer(gl, fg_attr, 3, FLOAT, false, INSTANCE_STRIDE, 32);
            ffi::gl_vertex_attrib_divisor(gl, fg_attr, 1);

            ffi::gl_enable_vertex_attrib_array(gl, bg_attr);
            ffi::gl_vertex_attrib_pointer(gl, bg_attr, 3, FLOAT, false, INSTANCE_STRIDE, 44);
            ffi::gl_vertex_attrib_divisor(gl, bg_attr, 1);

            ffi::gl_enable_vertex_attrib_array(gl, sel_attr);
            ffi::gl_vertex_attrib_pointer(gl, sel_attr, 1, FLOAT, false, INSTANCE_STRIDE, 56);
            ffi::gl_vertex_attrib_divisor(gl, sel_attr, 1);

            ffi::gl_enable_vertex_attrib_array(gl, cur_attr);
            ffi::gl_vertex_attrib_pointer(gl, cur_attr, 1, FLOAT, false, INSTANCE_STRIDE, 60);
            ffi::gl_vertex_attrib_divisor(gl, cur_attr, 1);
        };

        // --- Pass 1: Background rects (mode = 0) ---
        ffi::gl_bind_buffer(gl, ARRAY_BUFFER, self.brush.bg_instance_buffer);
        ffi::gl_buffer_data_f32(gl, ARRAY_BUFFER, &bg_instances, DYNAMIC_DRAW);
        ffi::gl_uniform1f(gl, self.brush.mode_loc, 0.0);
        bind_instances(gl, self.brush.bg_instance_buffer);
        ffi::gl_draw_arrays_instanced(gl, TRIANGLE_STRIP, 0, 4, bg_count as i32);

        // --- Pass 2: Text (mode = 1) ---
        ffi::gl_bind_buffer(gl, ARRAY_BUFFER, self.brush.text_instance_buffer);
        ffi::gl_buffer_data_f32(gl, ARRAY_BUFFER, &text_instances, DYNAMIC_DRAW);
        ffi::gl_uniform1f(gl, self.brush.mode_loc, 1.0);
        bind_instances(gl, self.brush.text_instance_buffer);
        ffi::gl_draw_arrays_instanced(gl, TRIANGLE_STRIP, 0, 4, text_count as i32);

        ffi::gl_disable_vertex_attrib_array(gl, off_attr);
        ffi::gl_disable_vertex_attrib_array(gl, size_attr);
        ffi::gl_disable_vertex_attrib_array(gl, uv_attr);
        ffi::gl_disable_vertex_attrib_array(gl, fg_attr);
        ffi::gl_disable_vertex_attrib_array(gl, bg_attr);
        ffi::gl_disable_vertex_attrib_array(gl, sel_attr);
        ffi::gl_disable_vertex_attrib_array(gl, cur_attr);

        Ok(())
    }

    fn build_instances(
        rows: u32,
        cols: u32,
        cell_w: u32,
        cell_h: u32,
        atlas: &GlyphAtlas,
        screen: &vt100::Screen,
        prows: u16,
        pcols: u16,
        selection: &[(u16, u16)],
        cursor: (u16, u16),
        default_fg: u32,
        default_bg: u32,
        dpr: f64,
    ) -> (Vec<f32>, Vec<f32>) {
        let sel_set: std::collections::HashSet<(u16, u16)> = selection.iter().copied().collect();
        let capacity = (rows * cols) as usize * INSTANCE_FLOATS;
        let mut bg = Vec::with_capacity(capacity);
        let mut text = Vec::with_capacity(capacity * 2);
        let solid = atlas.solid_uv();
        let cell_wf = cell_w as f32;
        let cell_hf = cell_h as f32;

        for r in 0..rows {
            for c in 0..cols {
                let px = c * cell_w;
                // Both renderers share the top-down pixel convention: row 0 is
                // the canvas top and each row steps one cell height downward.
                let py = r * cell_h;
                let is_selected = sel_set.contains(&(r as u16, c as u16));
                let is_cursor = (r as u16, c as u16) == cursor;

                let cell = if r < prows as u32 && c < pcols as u32 {
                    screen.cell(r as u16, c as u16)
                } else {
                    None
                };
                let ch = cell
                    .and_then(|cell| cell.contents().chars().next())
                    .unwrap_or(' ');

                // Shared cursor/selection color decision (same as Canvas 2D).
                let override_ = if is_cursor {
                    CellOverride::Cursor
                } else if is_selected {
                    CellOverride::Selected
                } else {
                    CellOverride::Normal
                };
                let (fg_rgb, bg_rgb) = cell_visual(cell, default_fg, default_bg, override_);
                let (fg_r, fg_g, fg_b) = rgb_to_floats(fg_rgb);
                let (bg_r, bg_g, bg_b) = rgb_to_floats(bg_rgb);

                let sel = if is_selected && !is_cursor { 1.0 } else { 0.0 };
                let cur = if is_cursor { 1.0 } else { 0.0 };

                // Background pass: one full-cell quad per cell
                bg.extend_from_slice(&[
                    px as f32, py as f32,   // offset
                    cell_wf, cell_hf,       // size
                    0.0, 0.0, 0.0, 0.0,     // UV (ignored in mode 0)
                    fg_r, fg_g, fg_b,       // foreground
                    bg_r, bg_g, bg_b,       // background
                    sel, cur,
                ]);

                // Text pass: glyph quad, or flat geometry for graphic cells
                if let Some(rects) =
                    graphic_cell_rects(ch, px as f64, py as f64, cell_wf as f64, cell_hf as f64, dpr)
                {
                    for (x, y, w, h, a) in rects {
                        let a = a as f32;
                        // Shaded blocks are pre-blended over the cell background;
                        // full-alpha glyphs use the foreground as-is.
                        let (tr, tg, tb) = if a < 1.0 {
                            (
                                fg_r * a + bg_r * (1.0 - a),
                                fg_g * a + bg_g * (1.0 - a),
                                fg_b * a + bg_b * (1.0 - a),
                            )
                        } else {
                            (fg_r, fg_g, fg_b)
                        };
                        text.extend_from_slice(&[
                            x as f32, y as f32, w as f32, h as f32, // geometry rect
                            solid.0, solid.1, solid.2, solid.3, // flat sample
                            tr, tg, tb,     // paint color
                            bg_r, bg_g, bg_b,
                            sel, cur,
                        ]);
                    }
                } else {
                    let (u0, v0, u1, v1) = atlas.uv_for(ch).unwrap_or((0.0, 0.0, 0.0, 0.0));
                    text.extend_from_slice(&[
                        px as f32, py as f32,   // offset
                        cell_wf, cell_hf,       // size
                        u0, v0, u1, v1,         // UV
                        fg_r, fg_g, fg_b,       // foreground
                        bg_r, bg_g, bg_b,       // background
                        sel, cur,
                    ]);
                }
            }
        }
        (bg, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_atlas() -> GlyphAtlas {
        GlyphAtlas {
            texture: 0,
            atlas_width: 320,
            atlas_height: 1000,
            glyph_width: 8,
            glyph_height: 18,
            uv_map: (0..1584).map(|_| (0.25, 0.25, 0.75, 0.75)).collect(),
        }
    }

    /// Every atlas range must be reachable through `uv_for` with correct
    /// (non-degenerate, non-zero-area) UVs.
    #[test]
    fn uv_for_resolves_every_atlas_range() {
        let atlas = scratch_atlas();
        for (cp, _note) in [
            (0x41u32, "ASCII"),       // 'A'
            (0x2888u32, "braille"),   // U+2888
            (0x25A3u32, "geo"),       // U+25A3
            (0x25AEu32, "geo/block"), // U+25AE
            (0x21BBu32, "arrows"),    // U+21BB
            (0x22EFu32, "mathops"),   // U+22EF
            (0x2731u32, "dingbats"),  // U+2731
            (0x2736u32, "dingbats"),  // U+2736
            (0x00B7u32, "latin1"),    // U+00B7
            (0x2026u32, "punct"),     // U+2026
            (0x20ACu32, "currency"),  // U+20AC
            (0x2699u32, "misc"),      // U+2699
        ] {
            let ch = char::from_u32(cp).unwrap();
            let uv = atlas
                .uv_for(ch)
                .unwrap_or_else(|| panic!("uv_for(U+{:04X}) = None", cp));
            assert!(uv.0 < uv.2 && uv.1 < uv.3, "U+{:04X} got degenerate UV {:?}", cp, uv);
        }
        // Out-of-range codepoints stay None (defaults to invisible).
        assert!(atlas.uv_for('\u{1F600}').is_none());
    }

    /// Without a DOM (host tests) the browser rasterizer must be a no-op: it
    /// neither panics nor scribbles on the atlas bitmap.
    #[test]
    fn rasterize_atlas_is_a_safe_noop_without_a_dom() {
        let mut data = vec![0u8; 8 * 18];
        GlyphAtlas::rasterize_atlas(&mut data, 8, 8, 18, 14.0);
        assert!(data.iter().all(|&b| b == 0), "atlas mutated without a DOM");
    }

    /// Braille glyphs must rasterize visible ink inside a production 8x18 slot,
    /// so the atlas slots aren't blank (which would render them invisible).
    #[test]
    fn braille_rasterizes_into_the_slot() {
        let (glyph_w, glyph_h): (u32, u32) = (8, 18);
        let mut any_ink = false;
        for cp in 0x2800u32..=0x28FF {
            let mut slot = vec![0u8; (glyph_w * glyph_h) as usize];
            GlyphAtlas::rasterize_braille(cp, glyph_w, glyph_h, &mut slot, 0, 0, glyph_w);
            if slot.iter().any(|&b| b > 20) {
                any_ink = true;
                break;
            }
        }
        assert!(any_ink, "no braille codepoint rasterizes any ink in an 8x18 slot");
    }

    /// opencode's "thinking" spinner cycles U+280B U+2819 U+2839 U+2838 U+283C
    /// U+2834 U+2826 U+2827 U+2807 U+280F at 80ms. The synthesized braille slots
    /// must each bake a *distinct* bitmap, otherwise the spinner looks frozen
    /// even though the screen updates.
    #[test]
    fn opencode_spinner_frames_rasterize_distinctly() {
        let frames = ['\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}', '\u{2807}', '\u{280F}'];
        for (glyph_w, glyph_h) in [(8u32, 18u32), (16, 36)] {
            let mut slots: Vec<Vec<u8>> = Vec::new();
            for ch in frames {
                let mut slot = vec![0u8; (glyph_w * glyph_h) as usize];
                GlyphAtlas::rasterize_braille(ch as u32, glyph_w, glyph_h, &mut slot, 0, 0, glyph_w);
                slots.push(slot);
            }
            for (i, a) in slots.iter().enumerate() {
                let ink_a = a.iter().filter(|&&v| v > 20).count();
                assert!(ink_a > 0, "frame {:?} (U+{:04X}) rasterizes blank at {}x{}", frames[i], frames[i] as u32, glyph_w, glyph_h);
                for (j, b) in slots.iter().enumerate() {
                    if i < j {
                        assert_ne!(
                            a, b,
                            "frames U+{:04X} and U+{:04X} rasterize identically at {}x{}",
                            frames[i] as u32, frames[j] as u32, glyph_w, glyph_h
                        );
                    }
                }
            }
        }
    }
}
