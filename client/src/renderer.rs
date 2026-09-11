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
// Color overrides (applied in build_instances):
//   selected: fg and bg are swapped
//   cursor: fg = original bg, bg = original fg (block cursor)

use ab_glyph::{Font, FontRef, Glyph, Point, PxScale, ScaleFont};

use crate::ffi::{self, JsHandle};

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
const FIRST_ASCII: char = '\u{0}';
const LAST_ASCII: char = '\u{7f}';
const ATLAS_COLS: usize = 16;

/// Embedded monospace font used for glyph rasterization.
///
/// Bundled into the WASM binary so the WebGL2 renderer doesn't depend on any
/// host fonts being installed. Sourced from Hack (a monospace font with solid
/// box-drawing/block glyph coverage).
pub const EMBEDDED_FONT: &[u8] = include_bytes!("../fonts/Hack-Regular.ttf");

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
    pub fn new(
        gl: JsHandle,
        font: &FontRef,
        cell_w: f64,
        cell_h: f64,
        dpr: f64,
    ) -> Result<Self, String> {
        let glyph_w = (cell_w * dpr).ceil() as u32;
        let glyph_h = (cell_h * dpr).ceil() as u32;

        // Scale the font so a glyph's *advance width* equals exactly one cell
        // width (`glyph_w` device px). Scaling em-height to the cell HEIGHT
        // instead makes Hack's ~0.606em advance wider than the 8px cell, so
        // glyph ink overflows the slot edge: wide glyphs touch the next cell
        // while narrow ones leave a visible gap. Painting every glyph at a
        // common advance == cell width reproduces the Canvas 2D renderer,
        // whose monospace system font advances exactly one cell per char.
        // For an 8px cell that is an ~13.2px em sitting inside the 18px slot.
let h_adv1 = {
                let scaled = font.as_scaled(PxScale::from(1.0));
                scaled.h_advance(font.glyph_id(' '))
            };
        let em = if h_adv1.is_finite() && h_adv1 > 0.0 {
            (glyph_w as f32 / h_adv1).max(1.0)
        } else {
            glyph_h as f32
        };

        // Row (from the top of the atlas slot) where glyphs' text baseline
        // lands, derived from the same `em` the glyphs are rasterized at.
        // Everything vertical is derived from this one number, so every
        // glyph shares a baseline instead of being top-aligned to its own
        // bounding box (which would park each glyph's baseline at a different
        // height). Hack has ascent+descent == em and line_gap == 0, so the
        // full glyph extents fit the slot with no clipping.
        //
        // The baseline centers the em box (ascent..descent) inside the
        // glyph_h slot, mirroring the Canvas 2D reference renderer, which
        // paints with `textBaseline: "middle"` (em box centered in the cell).
        // Parking the baseline at the cell top instead shifts the whole glyph
        // line ~5px upward relative to the 2D path, which the gl-vs-2d visual
        // comparison reads as "gl text is out of alignment".
        let baseline = {
            let scaled = font.as_scaled(PxScale::from(em));
            let asc = scaled.ascent();
            if asc.is_finite() && asc > 0.0 {
                (((glyph_h as f32 - em) / 2.0 + asc)
                    .round()
                    as i32)
                    .clamp(0, glyph_h as i32 - 1)
            } else {
                (glyph_h as i32) / 2
            }
        };

        let count = (LAST_ASCII as u32 - FIRST_ASCII as u32 + 1) as usize;
        let cols = ATLAS_COLS as u32;
        let rows = ((count as u32 + cols - 1) / cols) as u32;
        let atlas_w = cols * (glyph_w + ATLAS_PADDING);
        let atlas_h = rows * (glyph_h + ATLAS_PADDING);

        let mut data: Vec<u8> = vec![0; (atlas_w * atlas_h) as usize];
        let mut uv_map = Vec::with_capacity(count);

        for i in 0..count {
            let ch = char::from_u32(FIRST_ASCII as u32 + i as u32)
                .ok_or("invalid ASCII char")?;
            let col = (i % ATLAS_COLS) as u32;
            let row = (i / ATLAS_COLS) as u32;
            let x = col * (glyph_w + ATLAS_PADDING);
            let y = row * (glyph_h + ATLAS_PADDING);

            Self::rasterize_glyph(font, ch, glyph_w, glyph_h, em, baseline, &mut data, x, y, atlas_w);

            let u0 = x as f32 / atlas_w as f32;
            let v0 = y as f32 / atlas_h as f32;
            let u1 = (x + glyph_w) as f32 / atlas_w as f32;
            let v1 = (y + glyph_h) as f32 / atlas_h as f32;
            // Glyphs rasterize top-down into the data array, but the screen quad
            // samples v0 at its top edge. Swapping the row endpoints mirrors the
            // sample across the horizontal axis so glyphs render upright.
            uv_map.push((u0, v1, u1, v0));
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

fn rasterize_glyph(
        font: &FontRef,
        ch: char,
        glyph_w: u32,
        glyph_h: u32,
        em: f32,
        baseline: i32,
        data: &mut [u8],
        x: u32,
        y: u32,
        stride: u32,
    ) {
        let glyph = Glyph {
            id: font.glyph_id(ch),
            scale: PxScale::from(em),
            position: Point { x: 0.0, y: 0.0 },
        };
        if let Some(outlined) = font.outline_glyph(glyph) {
            let b = outlined.px_bounds();
            // (gx, gy) are relative to the glyph's own bounding box top-left.
            // In the glyph's image space the advance origin sits at (0, 0) (the
            // pen position), so a box pixel's image row is `b.min.y + gy` and
            // its image column is `b.min.x + gx`. Shift every pixel so the
            // baseline lands on the shared `baseline` slot row AND the ink
            // lands at its natural left side bearing (an advance-origin offset
            // of `b.min.x`), reproducing the Canvas 2D renderer, which parks
            // the pen at the cell's left edge and lets each glyph's own bearing
            // place its ink. Pinning the box left edge to the cell left instead
            // (flush-left) glues narrow glyphs like `|`, `.`, and `,` to the
            // cell edge, misaligning them against the reference renderer.
            // Pixels that fall outside the slot are clipped.
            let box_left = b.min.x as i32;
            let box_top = b.min.y as i32;
            outlined.draw(|gx, gy, coverage| {
                let slot_col = box_left + gx as i32;
                if slot_col >= 0 && (slot_col as u32) < glyph_w {
                    let slot_row = box_top + gy as i32 + baseline;
                    if slot_row >= 0 && (slot_row as u32) < glyph_h {
                        let px = x + slot_col as u32;
                        let py = y + slot_row as u32;
                        data[(py * stride + px) as usize] = (coverage * 255.0) as u8;
                    }
                }
            });
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
        let idx = (ch as u32).wrapping_sub(FIRST_ASCII as u32) as usize;
        self.uv_map.get(idx).copied()
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
        font: &FontRef,
        cell_w: f64,
        cell_h: f64,
        dpr: f64,
    ) -> Result<(), String> {
        let new_atlas = Self::new(gl, font, cell_w, cell_h, dpr)?;
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

/// Compute the device-pixel rects that paint a graphic glyph (box-drawing or
/// block element) inside a cell whose top-left pixel is `(px, py)`. The GL
/// renderer uses the same top-down pixel convention as the Canvas 2D path, so
/// this mirrors `draw_graphic_cell` exactly (line widths and the overdraw
/// epsilon are scaled by `dpr`). Returns `(x, y, w, h, alpha)` rects.
fn graphic_rects(
    ch: char,
    px: f32,
    py: f32,
    cw: f32,
    ch_h: f32,
    dpr: f64,
) -> Option<Vec<(f32, f32, f32, f32, f32)>> {
    use crate::graphics::{
        block_geometry, box_geometry, box_line_width, BarSide, StemSide, GRAPHIC_EPS,
    };
    let eps = (GRAPHIC_EPS * dpr) as f32;
    if let Some((fx0, fy0, fx1, fy1, alpha)) = block_geometry(ch) {
        let x = px + fx0 as f32 * cw - eps;
        let y = py + fy0 as f32 * ch_h - eps;
        let w = (fx1 - fx0) as f32 * cw + eps * 2.0;
        let h = (fy1 - fy0) as f32 * ch_h + eps * 2.0;
        return Some(vec![(x, y, w, h, alpha as f32)]);
    }
    let (bar, stem, weight) = box_geometry(ch)?;
    let s = dpr as f32;
    let (t_css, gap_css) = box_line_width(weight);
    let t = t_css as f32 * s;
    let gap = gap_css as f32 * s;
    let cx = px + cw * 0.5;
    let cy = py + ch_h * 0.5;
    let null_off = 0.0f32;
    let offsets: &[f32] = if gap > 0.0 { &[-gap, gap] } else { &[null_off] };
    let mut rects = Vec::with_capacity(4);
    for &off in offsets {
        if stem != StemSide::None {
            let x = cx + off - t * 0.5;
            let (y, h) = match stem {
                StemSide::Full => (py - eps, ch_h + eps * 2.0),
                StemSide::Up => (py - eps, cy - py + t * 0.5 + eps),
                StemSide::Down => {
                    let y = cy - t * 0.5 - eps;
                    (y, (py + ch_h) - (cy - t * 0.5 - eps) + eps)
                }
                _ => unreachable!(),
            };
            rects.push((x, y, t, h, 1.0));
        }
        if bar != BarSide::None {
            let y = cy + off - t * 0.5;
            let (x, w) = match bar {
                BarSide::Full => (px - eps, cw + eps * 2.0),
                BarSide::Left => (px - eps, cx - px + t * 0.5 + eps),
                BarSide::Right => {
                    let x = cx - t * 0.5 - eps;
                    (x, (px + cw) - (cx - t * 0.5 - eps) + eps)
                }
                _ => unreachable!(),
            };
            rects.push((x, y, w, t, 1.0));
        }
    }
    Some(rects)
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
    pub font_bytes: Vec<u8>,
}

impl WebGL2Renderer {
    pub fn new(
        canvas_id: &str,
        cell_w: f64,
        cell_h: f64,
        rows: u16,
        cols: u16,
        dpr: f64,
        font_bytes: &[u8],
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

        let font_ref = FontRef::try_from_slice(font_bytes)
            .map_err(|e| format!("font load failed: {:?}", e))?;
        let font_check = font_ref.clone();

        let atlas = GlyphAtlas::new(gl, &font_check, cell_w, cell_h, dpr)?;
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
            font_bytes: font_bytes.to_vec(),
        })
    }

    pub fn rebuild_atlas(&mut self) -> Result<(), String> {
        let font = FontRef::try_from_slice(&self.font_bytes)
            .map_err(|e| format!("font reload failed: {:?}", e))?;
        let css_w = self.cell_w as f64 / self.dpr;
        let css_h = self.cell_h as f64 / self.dpr;
        self.atlas.rebuild(self.ctx, &font, css_w, css_h, self.dpr)
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

                let ch = if r < prows as u32 && c < pcols as u32 {
                    screen
                        .cell(r as u16, c as u16)
                        .map(|cell| cell.contents().chars().next().unwrap_or(' '))
                        .unwrap_or(' ')
                } else {
                    ' '
                };

                let (mut fg_r, mut fg_g, mut fg_b, mut bg_r, mut bg_g, mut bg_b) =
                    if r < prows as u32 && c < pcols as u32 {
                        if let Some(cell) = screen.cell(r as u16, c as u16) {
                            let fg_rgb = crate::color::cell_fg_rgb(&cell, default_fg);
                            let bg_rgb = crate::color::color_to_rgb(cell.bgcolor(), default_bg);
                            let (fr, fg, fb) = rgb_to_floats(fg_rgb);
                            let (br, bg, bb) = rgb_to_floats(bg_rgb);
                            (fr, fg, fb, br, bg, bb)
                        } else {
                            let (fr, fg, fb) = rgb_to_floats(default_fg);
                            let (br, bg, bb) = rgb_to_floats(default_bg);
                            (fr, fg, fb, br, bg, bb)
                        }
                    } else {
                        let (fr, fg, fb) = rgb_to_floats(default_fg);
                        let (br, bg, bb) = rgb_to_floats(default_bg);
                        (fr, fg, fb, br, bg, bb)
                    };

                // Apply visual overrides (cursor takes priority over selection)
                if is_cursor {
                    // Block cursor: swap fg and bg
                    let (t_r, t_g, t_b) = (fg_r, fg_g, fg_b);
                    fg_r = bg_r;
                    fg_g = bg_g;
                    fg_b = bg_b;
                    bg_r = t_r;
                    bg_g = t_g;
                    bg_b = t_b;
                } else if is_selected {
                    // Selection: background = original fg, text = black
                    bg_r = fg_r;
                    bg_g = fg_g;
                    bg_b = fg_b;
                    fg_r = 0.0;
                    fg_g = 0.0;
                    fg_b = 0.0;
                }

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
                if let Some(rects) = graphic_rects(ch, px as f32, py as f32, cell_wf, cell_hf, dpr) {
                    for (x, y, w, h, a) in rects {
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
                            x, y, w, h,     // geometry rect
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
    use ab_glyph::ScaleFont;

    fn font() -> FontRef<'static> {
        FontRef::try_from_slice(EMBEDDED_FONT).unwrap()
    }

    /// Mirror the production em/baseline derivation: rasterize at the em where
    /// the font's advance equals one `glyph_w` cell, so every character advances
    /// exactly one cell (the Canvas 2D monospace invariant).
    fn em_and_baseline(f: &FontRef, glyph_w: u32, glyph_h: u32) -> (f32, i32) {
        let adv1 = f.as_scaled(PxScale::from(1.0)).h_advance(f.glyph_id(' '));
        let em = if adv1.is_finite() && adv1 > 0.0 {
            (glyph_w as f32 / adv1).max(1.0)
        } else {
            glyph_h as f32
        };
        let asc = f.as_scaled(PxScale::from(em)).ascent();
        let baseline = if asc.is_finite() && asc > 0.0 {
            ((((glyph_h as f32 - em) / 2.0 + asc).round() as i32))
            .clamp(0, glyph_h as i32 - 1)
        } else {
            (glyph_h / 2) as i32
        };
        (em, baseline)
    }

    /// Rasterize `ch` into a `glyph_w x glyph_h` scratch slot using the
    /// production scale and return the ink bounding box in slot rows.
    fn ink_bounds(ch: char, glyph_w: u32, glyph_h: u32) -> (i32, i32) {
        let f = font();
        let (em, baseline) = em_and_baseline(&f, glyph_w, glyph_h);
        let mut slot = vec![0u8; (glyph_w * glyph_h) as usize];
        GlyphAtlas::rasterize_glyph(&f, ch, glyph_w, glyph_h, em, baseline, &mut slot, 0, 0, glyph_w);
        let mut top = glyph_h as i32;
        let mut bottom = -1i32;
        for r in 0..glyph_h {
            for c in 0..glyph_w {
                if slot[(r * glyph_w + c) as usize] > 20 {
                    top = top.min(r as i32);
                    bottom = r as i32;
                }
            }
        }
        (top, bottom)
    }

    #[test]
    fn all_ascii_glyphs_share_one_baseline_row() {
        let f = font();
        let (glyph_w, glyph_h): (u32, u32) = (8, 18); // production config
        let (em, baseline) = em_and_baseline(&f, glyph_w, glyph_h);
        for ch in ['x', 'T', 'g', 'M', 'l', 'p', '|', ',', '_', '"', '^', '`'] {
            let (top, bottom) = ink_bounds(ch, glyph_w, glyph_h);
            assert!(
                top >= 0 && bottom >= top && bottom <= glyph_h as i32 - 1,
                "'{}' out of slot: top={} bottom={}",
                ch,
                top,
                bottom
            );
            // Same baseline as every other glyph: the box top of a glyph whose
            // box min.y is `my` must sit at `baseline + min.y` exactly.
            let glyph = Glyph {
                id: f.glyph_id(ch),
                scale: PxScale::from(em),
                position: Point { x: 0.0, y: 0.0 },
            };
            if let Some(ol) = f.outline_glyph(glyph) {
                let min_y = ol.px_bounds().min.y as i32;
                assert_eq!(
                    top,
                    baseline + min_y,
                    "'{}' not aligned to shared baseline",
                    ch
                );
            }
        }
    }

    #[test]
    fn no_descender_glyphs_sit_on_the_baseline() {
        // x-height/cap-height glyphs end exactly at the baseline row, so their
        // last ink row is `baseline - 1`. Descenders (`g`/`p`/`|`) must extend
        // below the baseline instead of sharing the box-top alignment.
        let (glyph_w, glyph_h): (u32, u32) = (8, 18);
        let (_em, baseline) = em_and_baseline(&font(), glyph_w, glyph_h);
        for ch in ['x', 'T', 'M'] {
            let (top, bottom) = ink_bounds(ch, glyph_w, glyph_h);
            assert_eq!(bottom, baseline - 1, "'{}' should end at the baseline", ch);
            assert!(top > 0, "'{}' should sit above the baseline", ch);
        }
        for ch in ['g', 'p', '|', 'y'] {
            let (_top, bottom) = ink_bounds(ch, glyph_w, glyph_h);
            assert!(bottom > baseline, "'{}' should descend below the baseline", ch);
        }
    }

    /// Rasterize `ch` into a `glyph_w x glyph_h` scratch slot using the
    /// production scale and return the ink bounding box in slot columns.
    fn ink_cols(ch: char, glyph_w: u32, glyph_h: u32) -> (i32, i32) {
        let f = font();
        let (em, baseline) = em_and_baseline(&f, glyph_w, glyph_h);
        let mut slot = vec![0u8; (glyph_w * glyph_h) as usize];
        GlyphAtlas::rasterize_glyph(&f, ch, glyph_w, glyph_h, em, baseline, &mut slot, 0, 0, glyph_w);
        let mut left = glyph_w as i32;
        let mut right = -1i32;
        for r in 0..glyph_h {
            for c in 0..glyph_w {
                if slot[(r * glyph_w + c) as usize] > 20 {
                    left = left.min(c as i32);
                    right = right.max(c as i32);
                }
            }
        }
        (left, right)
    }

    #[test]
    fn ink_sits_at_the_natural_left_side_bearing() {
        // The Canvas 2D renderer parks the advance origin at the cell's left
        // edge and lets each glyph's own left side bearing place the ink (each
        // glyph advances exactly one cell, but its ink is offset by `b.min.x`).
        // The atlas must reproduce that instead of flushing every glyph against
        // the cell's left edge. Allow one sub-pixel column of anti-aliasing
        // sliver at the box edge (coverage <= 20/255).
        let f = font();
        let (glyph_w, glyph_h): (u32, u32) = (8, 18); // production config
        let (em, _baseline) = em_and_baseline(&f, glyph_w, glyph_h);
        for ch in ['M', 'W', 'i', 'l', '|', ',', '.', 'o', 'x', 'T', '`', '-', '\'', '/'] {
            let (left, right) = ink_cols(ch, glyph_w, glyph_h);
            let glyph = Glyph {
                id: f.glyph_id(ch),
                scale: PxScale::from(em),
                position: Point { x: 0.0, y: 0.0 },
            };
            if let Some(ol) = f.outline_glyph(glyph) {
                let min_x = ol.px_bounds().min.x as i32;
                assert!(
                    left >= min_x,
                    "'{}' ink starts left of its side bearing",
                    ch
                );
                let drift = left - min_x;
                assert!(
                    drift <= 1 && (left > 0 || min_x == 0),
                    "'{}' ink flush-left at col 0 instead of its bearing {} (drift={})",
                    ch,
                    min_x,
                    drift
                );
                assert!(
                    right < glyph_w as i32,
                    "'{}' ink overflows the slot on the right",
                    ch
                );
            }
        }
        // Narrow glyphs with big bearings must be visibly inset from the cell
        // edge (this is the regression this test guards against).
        for (ch, min_inset) in [('|', 2), (',', 1), ('.', 1), ('i', 0)] {
            let (left, _right) = ink_cols(ch, glyph_w, glyph_h);
            assert!(
                left >= min_inset,
                "'{}' should sit {}px in from the cell left edge, got col {}",
                ch,
                min_inset,
                left
            );
        }
    }

    #[test]
    fn underscore_renders_below_the_baseline() {
        // '_' is entirely within the descent zone; it must not be glued to the
        // cell top like every other glyph is not.
        let (glyph_w, glyph_h): (u32, u32) = (8, 18);
        let (_em, baseline) = em_and_baseline(&font(), glyph_w, glyph_h);
        let (top, bottom) = ink_bounds('_', glyph_w, glyph_h);
        assert!(
            bottom > baseline && bottom <= glyph_h as i32 - 1,
            "underscore must descend below the baseline: rows [{}, {}]",
            top,
            bottom
        );
    }

    #[test]
    fn baseline_hydrates_at_other_sizes() {
        let f = font();
        let (glyph_w, glyph_h): (u32, u32) = (16, 36); // dpr=2
        let (_em, baseline) = em_and_baseline(&f, glyph_w, glyph_h);
        let (t_top, t_bottom) = ink_bounds('T', glyph_w, glyph_h);
        let (_g_top, g_bottom) = ink_bounds('g', glyph_w, glyph_h);
        assert_eq!(t_bottom, baseline - 1);
        assert!(t_top >= 0);
        assert!(g_bottom >= baseline, "g must descend below baseline");
        assert!(g_bottom <= glyph_h as i32 - 1);
    }

    /// `py` is the pixel y of the cell's TOP edge (top-down convention, shared
    /// with the Canvas 2D renderer). Return the midpoint of a rect on that axis.
    fn rect_vcenter(r: &(f32, f32, f32, f32, f32), ch_h: f32, py: f32) -> f32 {
        r.1 + r.3 * 0.5
    }

    #[test]
    fn block_geometry_paints_upward_not_mirrored() {
        let (px, py, cw, ch_h, dpr) = (0.0f32, 100.0f32, 8.0f32, 18.0f32, 1.0);
        let mid = py + ch_h * 0.5;
        // ▀ upper-half block: rect center must sit in the visual UPPER half.
        let up = graphic_rects('\u{2580}', px, py, cw, ch_h, dpr).unwrap();
        assert!(
            rect_vcenter(&up[0], ch_h, py) < mid,
            "▀ (upper half) painted below mid: center {} in the lower half",
            rect_vcenter(&up[0], ch_h, py)
        );
        // ▄ lower-half block: rect center must sit in the visual LOWER half.
        let down = graphic_rects('\u{2584}', px, py, cw, ch_h, dpr).unwrap();
        assert!(
            rect_vcenter(&down[0], ch_h, py) >= mid,
            "▄ (lower half) painted above mid: center {} in the upper half",
            rect_vcenter(&down[0], ch_h, py)
        );
        // █ full block spans the whole cell (centered on the middle).
        let full = graphic_rects('\u{2588}', px, py, cw, ch_h, dpr).unwrap();
        assert!((rect_vcenter(&full[0], ch_h, py) - mid).abs() < 1.0);
    }

    #[test]
    fn box_corners_point_the_right_way_up() {
        let (px, py, cw, ch_h, dpr) = (0.0f32, 100.0f32, 8.0f32, 18.0f32, 1.0);
        let mid = py + ch_h * 0.5;
        // ┌ (right bar + UP stem): the stem must sit in the top half; └ (right
        // bar + DOWN stem): the stem must sit in the bottom half. A vertically
        // flipped renderer swaps them (┌ renders as └).
        let tl = graphic_rects('\u{250C}', px, py, cw, ch_h, dpr).unwrap();
        let bl = graphic_rects('\u{2514}', px, py, cw, ch_h, dpr).unwrap();
        // First rect is the stem (bar rects are horizontal; only one arm each).
        assert!(
            rect_vcenter(&tl[0], ch_h, py) < mid,
            "┌ stem must point up, painted below mid {}",
            mid
        );
        assert!(
            rect_vcenter(&bl[0], ch_h, py) >= mid,
            "└ stem must point down, painted above mid {}",
            mid
        );
    }
}
