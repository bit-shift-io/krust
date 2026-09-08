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

use ab_glyph::{Font, FontRef, Glyph, Point, PxScale};
use js_sys::Float32Array;
use wasm_bindgen::prelude::*;
use web_sys::{
    WebGl2RenderingContext, WebGlBuffer, WebGlProgram, WebGlShader, WebGlTexture,
    WebGlUniformLocation,
};

const ATLAS_PADDING: u32 = 2;
const FIRST_ASCII: char = ' ';
const LAST_ASCII: char = '~';
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
    gl_Position = vec4(scaled / u_resolution * 2.0 - 1.0, 0.0, 1.0);
    v_texcoord = mix(a_uv.xy, a_uv.zw, a_texcoord);
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
    pub texture: WebGlTexture,
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub glyph_width: u32,
    pub glyph_height: u32,
    pub uv_map: Vec<(f32, f32, f32, f32)>,
}

impl GlyphAtlas {
    pub fn new(
        ctx: &WebGl2RenderingContext,
        font: &FontRef,
        cell_w: f64,
        cell_h: f64,
        dpr: f64,
    ) -> Result<Self, String> {
        let glyph_w = (cell_w * dpr).ceil() as u32;
        let glyph_h = (cell_h * dpr).ceil() as u32;

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

            Self::rasterize_glyph(font, ch, glyph_w, glyph_h, &mut data, x, y, atlas_w);

            let u0 = x as f32 / atlas_w as f32;
            let v0 = y as f32 / atlas_h as f32;
            let u1 = (x + glyph_w) as f32 / atlas_w as f32;
            let v1 = (y + glyph_h) as f32 / atlas_h as f32;
            uv_map.push((u0, v0, u1, v1));
        }

        // Reserve the bottom-right padding texel as an opaque "solid" sample:
        // graphic cells (box drawing / block elements) point their UVs here so
        // the text pass paints a flat foreground color instead of a glyph.
        data[((atlas_h - 1) * atlas_w + (atlas_w - 1)) as usize] = 255;

        let texture = Self::upload_texture(ctx, &data, atlas_w, atlas_h)?;

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
        data: &mut [u8],
        x: u32,
        y: u32,
        stride: u32,
    ) {
        let id = font.glyph_id(ch);
        let glyph = Glyph {
            id,
            scale: PxScale::from(glyph_h as f32),
            position: Point { x: 0.0, y: 0.0 },
        };
        if let Some(outlined) = font.outline_glyph(glyph) {
            outlined.draw(|gx, gy, coverage| {
                if gx < glyph_w && gy < glyph_h {
                    let px = x + gx;
                    let py = y + gy;
                    data[(py * stride + px) as usize] = (coverage * 255.0) as u8;
                }
            });
        }
    }

    fn upload_texture(
        ctx: &WebGl2RenderingContext,
        data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<WebGlTexture, String> {
        let texture = ctx.create_texture().ok_or("create_texture")?;
        ctx.bind_texture(WebGl2RenderingContext::TEXTURE_2D, Some(&texture));
        ctx.tex_parameteri(WebGl2RenderingContext::TEXTURE_2D, WebGl2RenderingContext::TEXTURE_MIN_FILTER, WebGl2RenderingContext::NEAREST as i32);
        ctx.tex_parameteri(WebGl2RenderingContext::TEXTURE_2D, WebGl2RenderingContext::TEXTURE_MAG_FILTER, WebGl2RenderingContext::NEAREST as i32);
        ctx.tex_parameteri(WebGl2RenderingContext::TEXTURE_2D, WebGl2RenderingContext::TEXTURE_WRAP_S, WebGl2RenderingContext::CLAMP_TO_EDGE as i32);
        ctx.tex_parameteri(WebGl2RenderingContext::TEXTURE_2D, WebGl2RenderingContext::TEXTURE_WRAP_T, WebGl2RenderingContext::CLAMP_TO_EDGE as i32);

        ctx.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            WebGl2RenderingContext::TEXTURE_2D,
            0,
            WebGl2RenderingContext::ALPHA as i32,
            width as i32,
            height as i32,
            0,
            WebGl2RenderingContext::ALPHA,
            WebGl2RenderingContext::UNSIGNED_BYTE,
            Some(data),
        )
        .map_err(|e| format!("tex_image_2d failed: {:?}", e))?;
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
        ctx: &WebGl2RenderingContext,
        font: &FontRef,
        cell_w: f64,
        cell_h: f64,
        dpr: f64,
    ) -> Result<(), String> {
        let new_atlas = Self::new(ctx, font, cell_w, cell_h, dpr)?;
        ctx.delete_texture(Some(&self.texture));
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
    pub program: WebGlProgram,
    pub pos_buffer: WebGlBuffer,
    pub uv_buffer: WebGlBuffer,
    pub bg_instance_buffer: WebGlBuffer,
    pub text_instance_buffer: WebGlBuffer,
    pub resolution_loc: WebGlUniformLocation,
    pub atlas_loc: WebGlUniformLocation,
    pub mode_loc: WebGlUniformLocation,
}

impl GlyphBrush {
    pub fn new(ctx: &WebGl2RenderingContext) -> Result<Self, String> {
        let program = Self::compile_program(ctx)?;

        let pos_buffer = ctx.create_buffer().ok_or("pos_buffer")?;
        let uv_buffer = ctx.create_buffer().ok_or("uv_buffer")?;
        let bg_instance_buffer = ctx.create_buffer().ok_or("bg_instance_buffer")?;
        let text_instance_buffer = ctx.create_buffer().ok_or("text_instance_buffer")?;

        let quad_verts: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
        let quad_uvs: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];

        ctx.bind_buffer(WebGl2RenderingContext::ARRAY_BUFFER, Some(&pos_buffer));
        ctx.buffer_data_with_array_buffer_view(
            WebGl2RenderingContext::ARRAY_BUFFER,
            &Float32Array::from(&quad_verts[..]),
            WebGl2RenderingContext::STATIC_DRAW,
        );

        ctx.bind_buffer(WebGl2RenderingContext::ARRAY_BUFFER, Some(&uv_buffer));
        ctx.buffer_data_with_array_buffer_view(
            WebGl2RenderingContext::ARRAY_BUFFER,
            &Float32Array::from(&quad_uvs[..]),
            WebGl2RenderingContext::STATIC_DRAW,
        );

        let resolution_loc = ctx.get_uniform_location(&program, "u_resolution")
            .ok_or("u_resolution")?;
        let atlas_loc = ctx.get_uniform_location(&program, "u_atlas")
            .ok_or("u_atlas")?;
        let mode_loc = ctx.get_uniform_location(&program, "u_mode")
            .ok_or("u_mode")?;

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

    fn compile_program(ctx: &WebGl2RenderingContext) -> Result<WebGlProgram, String> {
        let vs = Self::compile_shader(ctx, WebGl2RenderingContext::VERTEX_SHADER, VERTEX_SHADER)?;
        let fs = Self::compile_shader(ctx, WebGl2RenderingContext::FRAGMENT_SHADER, FRAGMENT_SHADER)?;
        let program = ctx.create_program().ok_or("create_program")?;
        ctx.attach_shader(&program, &vs);
        ctx.attach_shader(&program, &fs);
        ctx.link_program(&program);
        let linked = ctx.get_program_parameter(&program, WebGl2RenderingContext::LINK_STATUS).as_bool().unwrap_or(false);
        if !linked {
            let log = ctx.get_program_info_log(&program).unwrap_or_default();
            return Err(format!("link_program failed: {}", log));
        }
        Ok(program)
    }

    fn compile_shader(ctx: &WebGl2RenderingContext, kind: u32, source: &str) -> Result<WebGlShader, String> {
        let shader = ctx.create_shader(kind).ok_or("create_shader")?;
        ctx.shader_source(&shader, source);
        ctx.compile_shader(&shader);
        let compiled = ctx.get_shader_parameter(&shader, WebGl2RenderingContext::COMPILE_STATUS).as_bool().unwrap_or(false);
        if !compiled {
            let log = ctx.get_shader_info_log(&shader).unwrap_or_default();
            return Err(format!("compile_shader failed: {}", log));
        }
        Ok(shader)
    }
}

/// Split a default fg/bg color into (fr,fg,fb, br,bg,bb) as unit floats.
fn default_colors(default_fg: u32, default_bg: u32) -> (f32, f32, f32, f32, f32, f32) {
    (
        ((default_fg >> 16) & 0xff) as f32 / 255.0,
        ((default_fg >> 8) & 0xff) as f32 / 255.0,
        (default_fg & 0xff) as f32 / 255.0,
        ((default_bg >> 16) & 0xff) as f32 / 255.0,
        ((default_bg >> 8) & 0xff) as f32 / 255.0,
        (default_bg & 0xff) as f32 / 255.0,
    )
}

/// Compute the device-pixel rects that paint a graphic glyph (box-drawing or
/// block element) inside a cell starting at (px, py). Mirrors the Canvas 2D
/// `draw_graphic_cell` path, but in device pixels: line widths and the overdraw
/// epsilon are scaled by `dpr`. Returns `(x, y, w, h, alpha)` rects.
fn graphic_rects(
    ch: char,
    px: f32,
    py: f32,
    cw: f32,
    ch_h: f32,
    dpr: f64,
) -> Option<Vec<(f32, f32, f32, f32, f32)>> {
    use super::{
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
    pub ctx: WebGl2RenderingContext,
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
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(canvas_id))
            .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .ok_or_else(|| format!("canvas '#{}' not found", canvas_id))?;

        let gl = canvas
            .get_context("webgl2")
            .map_err(|e| format!("get_context(webgl2): {:?}", e))?
            .ok_or_else(|| "webgl2 context unavailable".to_string())?
            .dyn_into::<WebGl2RenderingContext>()
            .map_err(|_| "webgl2 cast failed".to_string())?;

        let font_ref = FontRef::try_from_slice(font_bytes)
            .map_err(|e| format!("font load failed: {:?}", e))?;
        let font_check = font_ref.clone();

        let atlas = GlyphAtlas::new(&gl, &font_check, cell_w, cell_h, dpr)?;
        let brush = GlyphBrush::new(&gl)?;

        Ok(WebGL2Renderer {
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
        self.atlas.rebuild(&self.ctx, &font, css_w, css_h, self.dpr)
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

        let gl = &self.ctx;
        let width = cols * self.cell_w;
        let height = rows * self.cell_h;

        gl.viewport(0, 0, width as i32, height as i32);
        gl.clear_color(
            ((default_bg >> 16) & 0xff) as f32 / 255.0,
            ((default_bg >> 8) & 0xff) as f32 / 255.0,
            ((default_bg) & 0xff) as f32 / 255.0,
            1.0,
        );
        gl.clear(WebGl2RenderingContext::COLOR_BUFFER_BIT);

        gl.use_program(Some(&self.brush.program));
        gl.uniform2f(Some(&self.brush.resolution_loc), width as f32, height as f32);

        gl.active_texture(WebGl2RenderingContext::TEXTURE0);
        gl.bind_texture(WebGl2RenderingContext::TEXTURE_2D, Some(&self.atlas.texture));
        gl.uniform1i(Some(&self.brush.atlas_loc), 0);

        let (bg_instances, text_instances) = Self::build_instances(
            rows, cols, self.cell_w, self.cell_h, &self.atlas, screen,
            prows, pcols, selection, cursor, default_fg, default_bg,
            self.dpr,
        );
        let bg_count = bg_instances.len() / INSTANCE_FLOATS;
        let text_count = text_instances.len() / INSTANCE_FLOATS;

        let pos_attr = gl.get_attrib_location(&self.brush.program, "a_position") as u32;
        let tex_attr = gl.get_attrib_location(&self.brush.program, "a_texcoord") as u32;
        let off_attr = gl.get_attrib_location(&self.brush.program, "a_offset") as u32;
        let size_attr = gl.get_attrib_location(&self.brush.program, "a_size") as u32;
        let uv_attr = gl.get_attrib_location(&self.brush.program, "a_uv") as u32;
        let fg_attr = gl.get_attrib_location(&self.brush.program, "a_fg") as u32;
        let bg_attr = gl.get_attrib_location(&self.brush.program, "a_bg") as u32;
        let sel_attr = gl.get_attrib_location(&self.brush.program, "a_sel") as u32;
        let cur_attr = gl.get_attrib_location(&self.brush.program, "a_cur") as u32;

        // Per-vertex attributes (shared across all instances)
        gl.bind_buffer(WebGl2RenderingContext::ARRAY_BUFFER, Some(&self.brush.pos_buffer));
        gl.enable_vertex_attrib_array(pos_attr);
        gl.vertex_attrib_pointer_with_i32(pos_attr, 2, WebGl2RenderingContext::FLOAT, false, 0, 0);

        gl.bind_buffer(WebGl2RenderingContext::ARRAY_BUFFER, Some(&self.brush.uv_buffer));
        gl.enable_vertex_attrib_array(tex_attr);
        gl.vertex_attrib_pointer_with_i32(tex_attr, 2, WebGl2RenderingContext::FLOAT, false, 0, 0);

        // Points the per-instance attributes at `buffer`. Called once per pass;
        // attribute pointers are stored per-attribute at pointer-set time, so
        // re-pointing with a different buffer bound switches the source.
        let bind_instances = |gl: &WebGl2RenderingContext, buffer: &WebGlBuffer| {
            gl.bind_buffer(WebGl2RenderingContext::ARRAY_BUFFER, Some(buffer));

            gl.enable_vertex_attrib_array(off_attr);
            gl.vertex_attrib_pointer_with_i32(off_attr, 2, WebGl2RenderingContext::FLOAT, false, INSTANCE_STRIDE, 0);
            gl.vertex_attrib_divisor(off_attr, 1);

            gl.enable_vertex_attrib_array(size_attr);
            gl.vertex_attrib_pointer_with_i32(size_attr, 2, WebGl2RenderingContext::FLOAT, false, INSTANCE_STRIDE, 8);
            gl.vertex_attrib_divisor(size_attr, 1);

            gl.enable_vertex_attrib_array(uv_attr);
            gl.vertex_attrib_pointer_with_i32(uv_attr, 4, WebGl2RenderingContext::FLOAT, false, INSTANCE_STRIDE, 16);
            gl.vertex_attrib_divisor(uv_attr, 1);

            gl.enable_vertex_attrib_array(fg_attr);
            gl.vertex_attrib_pointer_with_i32(fg_attr, 3, WebGl2RenderingContext::FLOAT, false, INSTANCE_STRIDE, 32);
            gl.vertex_attrib_divisor(fg_attr, 1);

            gl.enable_vertex_attrib_array(bg_attr);
            gl.vertex_attrib_pointer_with_i32(bg_attr, 3, WebGl2RenderingContext::FLOAT, false, INSTANCE_STRIDE, 44);
            gl.vertex_attrib_divisor(bg_attr, 1);

            gl.enable_vertex_attrib_array(sel_attr);
            gl.vertex_attrib_pointer_with_i32(sel_attr, 1, WebGl2RenderingContext::FLOAT, false, INSTANCE_STRIDE, 56);
            gl.vertex_attrib_divisor(sel_attr, 1);

            gl.enable_vertex_attrib_array(cur_attr);
            gl.vertex_attrib_pointer_with_i32(cur_attr, 1, WebGl2RenderingContext::FLOAT, false, INSTANCE_STRIDE, 60);
            gl.vertex_attrib_divisor(cur_attr, 1);
        };

        // --- Pass 1: Background rects (mode = 0) ---
        gl.bind_buffer(WebGl2RenderingContext::ARRAY_BUFFER, Some(&self.brush.bg_instance_buffer));
        gl.buffer_data_with_array_buffer_view(
            WebGl2RenderingContext::ARRAY_BUFFER,
            &Float32Array::from(&bg_instances[..]),
            WebGl2RenderingContext::DYNAMIC_DRAW,
        );
        gl.uniform1f(Some(&self.brush.mode_loc), 0.0);
        bind_instances(gl, &self.brush.bg_instance_buffer);
        gl.draw_arrays_instanced(
            WebGl2RenderingContext::TRIANGLE_STRIP,
            0,
            4,
            bg_count as i32,
        );

        // --- Pass 2: Text (mode = 1) ---
        gl.bind_buffer(WebGl2RenderingContext::ARRAY_BUFFER, Some(&self.brush.text_instance_buffer));
        gl.buffer_data_with_array_buffer_view(
            WebGl2RenderingContext::ARRAY_BUFFER,
            &Float32Array::from(&text_instances[..]),
            WebGl2RenderingContext::DYNAMIC_DRAW,
        );
        gl.uniform1f(Some(&self.brush.mode_loc), 1.0);
        bind_instances(gl, &self.brush.text_instance_buffer);
        gl.draw_arrays_instanced(
            WebGl2RenderingContext::TRIANGLE_STRIP,
            0,
            4,
            text_count as i32,
        );

        gl.disable_vertex_attrib_array(off_attr);
        gl.disable_vertex_attrib_array(size_attr);
        gl.disable_vertex_attrib_array(uv_attr);
        gl.disable_vertex_attrib_array(fg_attr);
        gl.disable_vertex_attrib_array(bg_attr);
        gl.disable_vertex_attrib_array(sel_attr);
        gl.disable_vertex_attrib_array(cur_attr);

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
                // The vertex shader maps pixel y=0 to NDC -1 (framebuffer
                // bottom), so terminal row 0 must be placed at the highest y.
                let py = (rows - 1 - r) * cell_h;
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
                            let fg_rgb = super::cell_fg_rgb(&cell, default_fg);
                            let bg_rgb = super::color_to_rgb(cell.bgcolor(), default_bg);
                            (
                                ((fg_rgb >> 16) & 0xff) as f32 / 255.0,
                                ((fg_rgb >> 8) & 0xff) as f32 / 255.0,
                                (fg_rgb & 0xff) as f32 / 255.0,
                                ((bg_rgb >> 16) & 0xff) as f32 / 255.0,
                                ((bg_rgb >> 8) & 0xff) as f32 / 255.0,
                                (bg_rgb & 0xff) as f32 / 255.0,
                            )
                        } else {
                            default_colors(default_fg, default_bg)
                        }
                    } else {
                        default_colors(default_fg, default_bg)
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
    #[test]
    fn atlas_builds_with_48_chars() {
        if let Ok(font_bytes) = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf") {
            assert!(!font_bytes.is_empty());
        }
    }
}
