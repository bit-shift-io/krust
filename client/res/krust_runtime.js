// krust_runtime.js — hand-maintained JS runtime for the krust WASM terminal.
//
// Implements the "krust" import module: a small, self-defined FFI surface so the
// raw WASM module has zero wasm-bindgen / web-sys / js-sys dependencies and can
// be instantiated with a plain WebAssembly.instantiate(Streaming) call plus
// `{ krust: KRUST_RUNTIME.imports.krust }` as the import object.
//
// Conventions:
//   - JS objects are referenced by `i32` handles into `heap` (index 0 = null).
//   - Strings travel from WASM as `(ptr, len)` into linear memory.
//   - Strings/bytes returned to WASM are written into caller-supplied buffers.
//   - All typed-array views are constructed fresh from `memory.buffer` because
//     the buffer detaches whenever WASM memory grows.
window.KRUST_RUNTIME = (function () {
  "use strict";

  let memory = null;
  let heap = [null];
  let freeSlots = [];

  function getObject(h) {
    return heap[h] || null;
  }

  function addObject(o) {
    if (!o) return 0;
    if (freeSlots.length) {
      const i = freeSlots.pop();
      heap[i] = o;
      return i;
    }
    heap.push(o);
    return heap.length - 1;
  }

  function release(h) {
    if (h > 0 && h < heap.length) {
      heap[h] = undefined;
      freeSlots.push(h);
    }
  }

  function readString(ptr, len) {
    return new TextDecoder().decode(new Uint8Array(memory.buffer, ptr, len));
  }

  function u8view(ptr, len) {
    return new Uint8Array(memory.buffer, ptr, len);
  }

  function f32view(ptr, count) {
    return new Float32Array(memory.buffer, ptr, count);
  }

  function writeStringTo(outPtr, outCap, s) {
    const bytes = new TextEncoder().encode(s);
    const n = Math.min(bytes.length, outCap);
    new Uint8Array(memory.buffer, outPtr, n).set(bytes.subarray(0, n));
    return n;
  }

  const krust = {
    // --- globals: window / document / elements ---------------------------
    krust_window: () => addObject(window),
    krust_window_dpr: (w) => (getObject(w) ? getObject(w).devicePixelRatio || 1 : 1),
    krust_window_document: (w) => (getObject(w) ? addObject(getObject(w).document) : 0),
    krust_document_get_element_by_id: (doc, p, l) =>
      addObject(getObject(doc).getElementById(readString(p, l))),
    krust_document_create_canvas: (doc) => addObject(getObject(doc).createElement("canvas")),
    krust_element_offset_width: (el) => getObject(el).offsetWidth || 0,
    krust_element_offset_height: (el) => getObject(el).offsetHeight || 0,
    krust_canvas_set_width: (c, w) => {
      getObject(c).width = w;
    },
    krust_canvas_set_height: (c, h) => {
      getObject(c).height = h;
    },
    krust_canvas_width: (c) => getObject(c).width,
    krust_canvas_height: (c) => getObject(c).height,
    krust_canvas_get_2d: (c) => addObject(getObject(c).getContext("2d")),
    krust_canvas_get_webgl2: (c) => addObject(getObject(c).getContext("webgl2")),
    krust_console_log: (p, l) => {
      console.log(readString(p, l));
    },
    krust_release: (h) => release(h),

    // --- Canvas 2D --------------------------------------------------------
    krust_ctx_set_transform: (ctx, a, b, c, d, e, f) => {
      getObject(ctx).setTransform(a, b, c, d, e, f);
    },
    krust_ctx_set_fill_style: (ctx, p, l) => {
      getObject(ctx).fillStyle = readString(p, l);
    },
    krust_ctx_set_global_alpha: (ctx, a) => {
      getObject(ctx).globalAlpha = a;
    },
    krust_ctx_fill_rect: (ctx, x, y, w, h) => {
      getObject(ctx).fillRect(x, y, w, h);
    },
    krust_ctx_set_font: (ctx, p, l) => {
      getObject(ctx).font = readString(p, l);
    },
    krust_ctx_set_text_baseline: (ctx, p, l) => {
      getObject(ctx).textBaseline = readString(p, l);
    },
    krust_ctx_fill_text: (ctx, p, l, x, y) => {
      getObject(ctx).fillText(readString(p, l), x, y);
    },
    krust_ctx_measure_text: (ctx, p, l) => addObject(getObject(ctx).measureText(readString(p, l))),
    krust_tm_width: (tm) => getObject(tm).width,
    krust_tm_ascent: (tm) => getObject(tm).actualBoundingBoxAscent,
    krust_tm_descent: (tm) => getObject(tm).actualBoundingBoxDescent,
    krust_ctx_get_image_data: (ctx, x, y, w, h, outPtr, outCap) => {
      const src = getObject(ctx).getImageData(x, y, w, h).data;
      const n = Math.min(src.length, outCap);
      u8view(outPtr, n).set(src.subarray(0, n));
      return n;
    },

    // --- WebGL2 -----------------------------------------------------------
    krust_gl_create_program: (gl) => addObject(getObject(gl).createProgram()),
    krust_gl_create_shader: (gl, kind) => addObject(getObject(gl).createShader(kind)),
    krust_gl_shader_source: (gl, sh, p, l) => {
      getObject(gl).shaderSource(getObject(sh), readString(p, l));
    },
    krust_gl_compile_shader: (gl, sh) => {
      getObject(gl).compileShader(getObject(sh));
    },
    krust_gl_get_shader_parameter: (gl, sh, pname) =>
      getObject(gl).getShaderParameter(getObject(sh), pname) ? 1 : 0,
    krust_gl_get_shader_info_log: (gl, sh, outPtr, outCap) =>
      writeStringTo(outPtr, outCap, getObject(gl).getShaderInfoLog(getObject(sh)) || ""),
    krust_gl_get_program_parameter: (gl, pr, pname) =>
      getObject(gl).getProgramParameter(getObject(pr), pname) ? 1 : 0,
    krust_gl_get_program_info_log: (gl, pr, outPtr, outCap) =>
      writeStringTo(outPtr, outCap, getObject(gl).getProgramInfoLog(getObject(pr)) || ""),
    krust_gl_attach_shader: (gl, pr, sh) => {
      getObject(gl).attachShader(getObject(pr), getObject(sh));
    },
    krust_gl_link_program: (gl, pr) => {
      getObject(gl).linkProgram(getObject(pr));
    },
    krust_gl_use_program: (gl, pr) => {
      getObject(gl).useProgram(getObject(pr));
    },
    krust_gl_create_buffer: (gl) => addObject(getObject(gl).createBuffer()),
    krust_gl_bind_buffer: (gl, target, buf) => {
      getObject(gl).bindBuffer(target, getObject(buf));
    },
    krust_gl_buffer_data_f32: (gl, target, ptr, count, usage) => {
      getObject(gl).bufferData(target, f32view(ptr, count), usage);
    },
    krust_gl_create_texture: (gl) => addObject(getObject(gl).createTexture()),
    krust_gl_bind_texture: (gl, target, tex) => {
      getObject(gl).bindTexture(target, getObject(tex));
    },
    krust_gl_tex_parameteri: (gl, target, pname, param) => {
      getObject(gl).texParameteri(target, pname, param);
    },
    krust_gl_tex_image_2d_alpha: (gl, target, level, ifmt, w, h, border, format, type, p, len) => {
      getObject(gl).texImage2D(target, level, ifmt, w, h, border, format, type, u8view(p, len));
    },
    krust_gl_active_texture: (gl, unit) => {
      getObject(gl).activeTexture(unit);
    },
    krust_gl_uniform1f: (gl, loc, f) => {
      getObject(gl).uniform1f(getObject(loc), f);
    },
    krust_gl_uniform1i: (gl, loc, i) => {
      getObject(gl).uniform1i(getObject(loc), i);
    },
    krust_gl_uniform2f: (gl, loc, a, b) => {
      getObject(gl).uniform2f(getObject(loc), a, b);
    },
    krust_gl_get_uniform_location: (gl, pr, p, l) =>
      addObject(getObject(gl).getUniformLocation(getObject(pr), readString(p, l))),
    krust_gl_get_attrib_location: (gl, pr, p, l) =>
      getObject(gl).getAttribLocation(getObject(pr), readString(p, l)),
    krust_gl_enable_vertex_attrib_array: (gl, index) => {
      getObject(gl).enableVertexAttribArray(index);
    },
    krust_gl_disable_vertex_attrib_array: (gl, index) => {
      getObject(gl).disableVertexAttribArray(index);
    },
    krust_gl_vertex_attrib_pointer: (gl, index, size, type, normalized, stride, offset) => {
      // offset crosses the WASM ABI as i64 → BigInt; WebGL needs a Number.
      getObject(gl).vertexAttribPointer(index, size, type, normalized, stride, Number(offset));
    },
    krust_gl_vertex_attrib_divisor: (gl, index, divisor) => {
      getObject(gl).vertexAttribDivisor(index, divisor);
    },
    krust_gl_draw_arrays_instanced: (gl, mode, first, count, instanceCount) => {
      getObject(gl).drawArraysInstanced(mode, first, count, instanceCount);
    },
    krust_gl_clear: (gl, mask) => {
      getObject(gl).clear(mask);
    },
    krust_gl_clear_color: (gl, r, g, b, a) => {
      getObject(gl).clearColor(r, g, b, a);
    },
    krust_gl_viewport: (gl, x, y, w, h) => {
      getObject(gl).viewport(x, y, w, h);
    },
    krust_gl_delete_texture: (gl, tex) => {
      getObject(gl).deleteTexture(getObject(tex));
    },
  };

  return {
    imports: { krust },
    install(mem) {
      memory = mem;
    },
  };
})();