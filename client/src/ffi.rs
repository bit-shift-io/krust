// Raw JS-interop FFI for the krust WASM client.
//
// The module imports a small, self-defined browser API from the JS module
// "krust" (implemented in `res/krust_runtime.js`) instead of depending on
// web-sys / js-sys / wasm-bindgen. All JS objects are referenced by `i32`
// handles into a JS-side registry; strings travel as `(*const u8, usize)`
// pairs; strings/bytes returned by JS are written into caller-supplied wasm
// buffers.
//
// These declarations only resolve to real imports on the `wasm32` target; on
// native targets they are inert declarations that are never linked (the
// browser-facing code paths are not reachable from unit tests).

/// Opaque handle into the JS-side object registry (0 = null).
pub(crate) type JsHandle = i32;

#[link(wasm_import_module = "krust")]
extern "C" {
    // --- globals: window / document / elements ---------------------------
    fn krust_window() -> i32;
    fn krust_window_dpr(w: i32) -> f64;
    fn krust_window_document(w: i32) -> i32;
    fn krust_document_get_element_by_id(doc: i32, ptr: *const u8, len: usize) -> i32;
    fn krust_document_create_canvas(doc: i32) -> i32;
    fn krust_element_offset_width(el: i32) -> f64;
    fn krust_element_offset_height(el: i32) -> f64;
    fn krust_canvas_set_width(c: i32, w: u32);
    fn krust_canvas_set_height(c: i32, h: u32);
    fn krust_canvas_width(c: i32) -> u32;
    fn krust_canvas_height(c: i32) -> u32;
    fn krust_canvas_get_2d(c: i32) -> i32;
    fn krust_canvas_get_webgl2(c: i32) -> i32;
    fn krust_console_log(ptr: *const u8, len: usize);
    fn krust_release(h: i32);

    // --- Canvas 2D --------------------------------------------------------
    fn krust_ctx_set_transform(ctx: i32, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64);
    fn krust_ctx_set_fill_style(ctx: i32, ptr: *const u8, len: usize);
    fn krust_ctx_set_global_alpha(ctx: i32, a: f64);
    fn krust_ctx_fill_rect(ctx: i32, x: f64, y: f64, w: f64, h: f64);
    fn krust_ctx_set_font(ctx: i32, ptr: *const u8, len: usize);
    fn krust_ctx_set_text_baseline(ctx: i32, ptr: *const u8, len: usize);
    fn krust_ctx_fill_text(ctx: i32, ptr: *const u8, len: usize, x: f64, y: f64);
    fn krust_ctx_measure_text(ctx: i32, ptr: *const u8, len: usize) -> i32;
    fn krust_tm_width(tm: i32) -> f64;
    fn krust_tm_ascent(tm: i32) -> f64;
    fn krust_tm_descent(tm: i32) -> f64;
    fn krust_ctx_get_image_data(ctx: i32, x: f64, y: f64, w: f64, h: f64, out: *mut u8, cap: usize) -> usize;

    // --- WebGL2 -----------------------------------------------------------
    fn krust_gl_create_program(gl: i32) -> i32;
    fn krust_gl_create_shader(gl: i32, kind: u32) -> i32;
    fn krust_gl_shader_source(gl: i32, sh: i32, ptr: *const u8, len: usize);
    fn krust_gl_compile_shader(gl: i32, sh: i32);
    fn krust_gl_get_shader_parameter(gl: i32, sh: i32, pname: u32) -> u32;
    fn krust_gl_get_shader_info_log(gl: i32, sh: i32, out: *mut u8, cap: usize) -> usize;
    fn krust_gl_get_program_parameter(gl: i32, pr: i32, pname: u32) -> u32;
    fn krust_gl_get_program_info_log(gl: i32, pr: i32, out: *mut u8, cap: usize) -> usize;
    fn krust_gl_attach_shader(gl: i32, pr: i32, sh: i32);
    fn krust_gl_link_program(gl: i32, pr: i32);
    fn krust_gl_use_program(gl: i32, pr: i32);
    fn krust_gl_create_buffer(gl: i32) -> i32;
    fn krust_gl_bind_buffer(gl: i32, target: u32, buf: i32);
    fn krust_gl_buffer_data_f32(gl: i32, target: u32, ptr: *const f32, count: usize, usage: u32);
    fn krust_gl_create_texture(gl: i32) -> i32;
    fn krust_gl_bind_texture(gl: i32, target: u32, tex: i32);
    fn krust_gl_tex_parameteri(gl: i32, target: u32, pname: u32, param: i32);
    fn krust_gl_tex_image_2d_alpha(
        gl: i32,
        target: u32,
        level: i32,
        internalformat: i32,
        w: i32,
        h: i32,
        border: i32,
        format: u32,
        type_: u32,
        ptr: *const u8,
        len: usize,
    );
    fn krust_gl_active_texture(gl: i32, unit: u32);
    fn krust_gl_uniform1f(gl: i32, loc: i32, f: f32);
    fn krust_gl_uniform1i(gl: i32, loc: i32, i: i32);
    fn krust_gl_uniform2f(gl: i32, loc: i32, a: f32, b: f32);
    fn krust_gl_get_uniform_location(gl: i32, pr: i32, ptr: *const u8, len: usize) -> i32;
    fn krust_gl_get_attrib_location(gl: i32, pr: i32, ptr: *const u8, len: usize) -> i32;
    fn krust_gl_enable_vertex_attrib_array(gl: i32, index: u32);
    fn krust_gl_disable_vertex_attrib_array(gl: i32, index: u32);
    fn krust_gl_vertex_attrib_pointer(
        gl: i32,
        index: u32,
        size: i32,
        type_: u32,
        normalized: bool,
        stride: i32,
        offset: i64,
    );
    fn krust_gl_vertex_attrib_divisor(gl: i32, index: u32, divisor: u32);
    fn krust_gl_draw_arrays_instanced(gl: i32, mode: u32, first: i32, count: i32, instance_count: i32);
    fn krust_gl_clear(gl: i32, mask: u32);
    fn krust_gl_clear_color(gl: i32, r: f32, g: f32, b: f32, a: f32);
    fn krust_gl_viewport(gl: i32, x: i32, y: i32, w: i32, h: i32);
    fn krust_gl_delete_texture(gl: i32, tex: i32);
}

fn ptr_len(s: &str) -> (*const u8, usize) {
    (s.as_ptr(), s.len())
}

// --- globals -------------------------------------------------------------

/// Handle to the `window` object (0 when unavailable).
pub(crate) fn window() -> JsHandle {
    unsafe { krust_window() }
}

/// `window.devicePixelRatio`, clamped to >= 1.0.
pub(crate) fn window_dpr(w: JsHandle) -> f64 {
    unsafe { krust_window_dpr(w) }.max(1.0)
}

/// `window.document` handle (0 when unavailable).
pub(crate) fn window_document(w: JsHandle) -> JsHandle {
    unsafe { krust_window_document(w) }
}

/// `document.getElementById(id)`.
pub(crate) fn document_get_element_by_id(doc: JsHandle, id: &str) -> JsHandle {
    let (p, l) = ptr_len(id);
    unsafe { krust_document_get_element_by_id(doc, p, l) }
}

/// `document.createElement("canvas")`.
pub(crate) fn document_create_canvas(doc: JsHandle) -> JsHandle {
    unsafe { krust_document_create_canvas(doc) }
}

pub(crate) fn element_offset_width(el: JsHandle) -> f64 {
    unsafe { krust_element_offset_width(el) }
}

pub(crate) fn element_offset_height(el: JsHandle) -> f64 {
    unsafe { krust_element_offset_height(el) }
}

pub(crate) fn canvas_set_width(c: JsHandle, w: u32) {
    unsafe { krust_canvas_set_width(c, w) }
}

pub(crate) fn canvas_set_height(c: JsHandle, h: u32) {
    unsafe { krust_canvas_set_height(c, h) }
}

pub(crate) fn canvas_width(c: JsHandle) -> u32 {
    unsafe { krust_canvas_width(c) }
}

pub(crate) fn canvas_height(c: JsHandle) -> u32 {
    unsafe { krust_canvas_height(c) }
}

/// `canvas.getContext("2d")` (0 when unavailable).
pub(crate) fn canvas_get_2d(c: JsHandle) -> JsHandle {
    unsafe { krust_canvas_get_2d(c) }
}

/// `canvas.getContext("webgl2")` (0 when unavailable).
pub(crate) fn canvas_get_webgl2(c: JsHandle) -> JsHandle {
    unsafe { krust_canvas_get_webgl2(c) }
}

/// Write a message to the browser console.
pub(crate) fn console_log(s: &str) {
    let (p, l) = ptr_len(s);
    unsafe { krust_console_log(p, l) }
}

/// Release a JS object handle from the registry.
pub(crate) fn release(h: JsHandle) {
    unsafe { krust_release(h) }
}

// --- Canvas 2D ------------------------------------------------------------

pub(crate) fn ctx_set_transform(ctx: JsHandle, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) {
    unsafe { krust_ctx_set_transform(ctx, a, b, c, d, e, f) }
}

pub(crate) fn ctx_set_fill_style(ctx: JsHandle, s: &str) {
    let (p, l) = ptr_len(s);
    unsafe { krust_ctx_set_fill_style(ctx, p, l) }
}

pub(crate) fn ctx_set_global_alpha(ctx: JsHandle, a: f64) {
    unsafe { krust_ctx_set_global_alpha(ctx, a) }
}

pub(crate) fn ctx_fill_rect(ctx: JsHandle, x: f64, y: f64, w: f64, h: f64) {
    unsafe { krust_ctx_fill_rect(ctx, x, y, w, h) }
}

pub(crate) fn ctx_set_font(ctx: JsHandle, s: &str) {
    let (p, l) = ptr_len(s);
    unsafe { krust_ctx_set_font(ctx, p, l) }
}

pub(crate) fn ctx_set_text_baseline(ctx: JsHandle, s: &str) {
    let (p, l) = ptr_len(s);
    unsafe { krust_ctx_set_text_baseline(ctx, p, l) }
}

pub(crate) fn ctx_fill_text(ctx: JsHandle, s: &str, x: f64, y: f64) {
    let (p, l) = ptr_len(s);
    unsafe { krust_ctx_fill_text(ctx, p, l, x, y) }
}

/// `ctx.measureText(s)`; returns a TextMetrics handle (release it after use).
pub(crate) fn ctx_measure_text(ctx: JsHandle, s: &str) -> JsHandle {
    let (p, l) = ptr_len(s);
    unsafe { krust_ctx_measure_text(ctx, p, l) }
}

pub(crate) fn tm_width(tm: JsHandle) -> f64 {
    unsafe { krust_tm_width(tm) }
}

pub(crate) fn tm_ascent(tm: JsHandle) -> f64 {
    unsafe { krust_tm_ascent(tm) }
}

pub(crate) fn tm_descent(tm: JsHandle) -> f64 {
    unsafe { krust_tm_descent(tm) }
}

/// Read `ImageData` RGBA pixels into `out`. Returns the number of bytes
/// written (0 when the readback is unavailable).
pub(crate) fn ctx_get_image_data(ctx: JsHandle, x: f64, y: f64, w: f64, h: f64, out: &mut [u8]) -> usize {
    unsafe { krust_ctx_get_image_data(ctx, x, y, w, h, out.as_mut_ptr(), out.len()) }
}

// --- WebGL2 ---------------------------------------------------------------

pub(crate) fn gl_create_program(gl: JsHandle) -> JsHandle {
    unsafe { krust_gl_create_program(gl) }
}

pub(crate) fn gl_create_shader(gl: JsHandle, kind: u32) -> JsHandle {
    unsafe { krust_gl_create_shader(gl, kind) }
}

pub(crate) fn gl_shader_source(gl: JsHandle, sh: JsHandle, src: &str) {
    let (p, l) = ptr_len(src);
    unsafe { krust_gl_shader_source(gl, sh, p, l) }
}

pub(crate) fn gl_compile_shader(gl: JsHandle, sh: JsHandle) {
    unsafe { krust_gl_compile_shader(gl, sh) }
}

pub(crate) fn gl_get_shader_parameter(gl: JsHandle, sh: JsHandle, pname: u32) -> u32 {
    unsafe { krust_gl_get_shader_parameter(gl, sh, pname) }
}

/// Copy the shader info log into `buf`; returns the byte length written.
pub(crate) fn gl_get_shader_info_log(gl: JsHandle, sh: JsHandle, buf: &mut [u8]) -> usize {
    unsafe { krust_gl_get_shader_info_log(gl, sh, buf.as_mut_ptr(), buf.len()) }
}

pub(crate) fn gl_get_program_parameter(gl: JsHandle, pr: JsHandle, pname: u32) -> u32 {
    unsafe { krust_gl_get_program_parameter(gl, pr, pname) }
}

pub(crate) fn gl_get_program_info_log(gl: JsHandle, pr: JsHandle, buf: &mut [u8]) -> usize {
    unsafe { krust_gl_get_program_info_log(gl, pr, buf.as_mut_ptr(), buf.len()) }
}

pub(crate) fn gl_attach_shader(gl: JsHandle, pr: JsHandle, sh: JsHandle) {
    unsafe { krust_gl_attach_shader(gl, pr, sh) }
}

pub(crate) fn gl_link_program(gl: JsHandle, pr: JsHandle) {
    unsafe { krust_gl_link_program(gl, pr) }
}

pub(crate) fn gl_use_program(gl: JsHandle, pr: JsHandle) {
    unsafe { krust_gl_use_program(gl, pr) }
}

pub(crate) fn gl_create_buffer(gl: JsHandle) -> JsHandle {
    unsafe { krust_gl_create_buffer(gl) }
}

pub(crate) fn gl_bind_buffer(gl: JsHandle, target: u32, buf: JsHandle) {
    unsafe { krust_gl_bind_buffer(gl, target, buf) }
}

pub(crate) fn gl_buffer_data_f32(gl: JsHandle, target: u32, data: &[f32], usage: u32) {
    unsafe { krust_gl_buffer_data_f32(gl, target, data.as_ptr(), data.len(), usage) }
}

pub(crate) fn gl_create_texture(gl: JsHandle) -> JsHandle {
    unsafe { krust_gl_create_texture(gl) }
}

pub(crate) fn gl_bind_texture(gl: JsHandle, target: u32, tex: JsHandle) {
    unsafe { krust_gl_bind_texture(gl, target, tex) }
}

pub(crate) fn gl_tex_parameteri(gl: JsHandle, target: u32, pname: u32, param: i32) {
    unsafe { krust_gl_tex_parameteri(gl, target, pname, param) }
}

/// `gl.texImage2D(..., UNSIGNED_BYTE, src)` with an `ALPHA`-format texture.
pub(crate) fn gl_tex_image_2d_alpha(
    gl: JsHandle,
    target: u32,
    level: i32,
    internalformat: i32,
    w: i32,
    h: i32,
    border: i32,
    format: u32,
    type_: u32,
    data: &[u8],
) {
    unsafe {
        krust_gl_tex_image_2d_alpha(
            gl, target, level, internalformat, w, h, border, format, type_,
            data.as_ptr(), data.len(),
        )
    }
}

pub(crate) fn gl_active_texture(gl: JsHandle, unit: u32) {
    unsafe { krust_gl_active_texture(gl, unit) }
}

pub(crate) fn gl_uniform1f(gl: JsHandle, loc: JsHandle, f: f32) {
    unsafe { krust_gl_uniform1f(gl, loc, f) }
}

pub(crate) fn gl_uniform1i(gl: JsHandle, loc: JsHandle, i: i32) {
    unsafe { krust_gl_uniform1i(gl, loc, i) }
}

pub(crate) fn gl_uniform2f(gl: JsHandle, loc: JsHandle, a: f32, b: f32) {
    unsafe { krust_gl_uniform2f(gl, loc, a, b) }
}

pub(crate) fn gl_get_uniform_location(gl: JsHandle, pr: JsHandle, name: &str) -> JsHandle {
    let (p, l) = ptr_len(name);
    unsafe { krust_gl_get_uniform_location(gl, pr, p, l) }
}

pub(crate) fn gl_get_attrib_location(gl: JsHandle, pr: JsHandle, name: &str) -> i32 {
    let (p, l) = ptr_len(name);
    unsafe { krust_gl_get_attrib_location(gl, pr, p, l) }
}

pub(crate) fn gl_enable_vertex_attrib_array(gl: JsHandle, index: u32) {
    unsafe { krust_gl_enable_vertex_attrib_array(gl, index) }
}

pub(crate) fn gl_disable_vertex_attrib_array(gl: JsHandle, index: u32) {
    unsafe { krust_gl_disable_vertex_attrib_array(gl, index) }
}

pub(crate) fn gl_vertex_attrib_pointer(
    gl: JsHandle,
    index: u32,
    size: i32,
    type_: u32,
    normalized: bool,
    stride: i32,
    offset: i64,
) {
    unsafe { krust_gl_vertex_attrib_pointer(gl, index, size, type_, normalized, stride, offset) }
}

pub(crate) fn gl_vertex_attrib_divisor(gl: JsHandle, index: u32, divisor: u32) {
    unsafe { krust_gl_vertex_attrib_divisor(gl, index, divisor) }
}

pub(crate) fn gl_draw_arrays_instanced(gl: JsHandle, mode: u32, first: i32, count: i32, instance_count: i32) {
    unsafe { krust_gl_draw_arrays_instanced(gl, mode, first, count, instance_count) }
}

pub(crate) fn gl_clear(gl: JsHandle, mask: u32) {
    unsafe { krust_gl_clear(gl, mask) }
}

pub(crate) fn gl_clear_color(gl: JsHandle, r: f32, g: f32, b: f32, a: f32) {
    unsafe { krust_gl_clear_color(gl, r, g, b, a) }
}

pub(crate) fn gl_viewport(gl: JsHandle, x: i32, y: i32, w: i32, h: i32) {
    unsafe { krust_gl_viewport(gl, x, y, w, h) }
}

pub(crate) fn gl_delete_texture(gl: JsHandle, tex: JsHandle) {
    unsafe { krust_gl_delete_texture(gl, tex) }
}