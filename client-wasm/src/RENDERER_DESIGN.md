# WebGL2 Glyph Atlas Renderer — Design Document

## Problem: Canvas 2D `fill_text()` Is Slow

The Canvas 2D renderer (`TerminalState::render()`) calls `fill_text()` per cell every frame. This forces the browser to rasterize each glyph on every draw — expensive even for 80×24 grids under heavy output. The approach was inherited from the original design where `beamterm-renderer` (WebGL2 with bitmap fonts) was planned, but bitmap fonts are blurry at non-integer scales (Firefox zoom, Retina displays, etc.).

## Solution: Pre-Rasterized Vector Font Atlas in WebGL2

**Render each glyph once at startup** using `ab_glyph`'s outline rasterization, upload the resulting bitmaps to a WebGL2 texture atlas, and draw the entire terminal in **one instanced draw call** per frame using textured quads. This gives:

- **Sharp fonts at any zoom**: rasterized at `cell_w × DPR` and `cell_h × DPR`, using nearest-neighbor filtering (pixel-perfect at integer DPR)
- **One draw call per frame**: `draw_arrays_instanced(TRIANGLE_STRIP, 0, 4, rows*cols)` — all cells rendered in a single GPU call
- **No per-frame font rasterization**: glyphs are pre-rasterized at startup and when the cell size or DPR changes

### Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    WebGL2 Renderer                         │
│                                                          │
│  ab_glyph::FontRef (TTF/OTF)                             │
│      │                                                   │
│      ├──► Rasterize 95 ASCII chars at cell_w*DPR × cell_h*DPR    │
│      │         ↓                                        │
│      │  GlyphAtlas (WebGL2 texture, nearest-neighbor)     │
│      │         ↓                                        │
│      │  uv_map[ch] → (u0,v0,u1,v1) per glyph            │
│      │                                                   │
│      └──► Draw: draw_arrays_instanced(TRIANGLE_STRIP,     │
│               0, 4, rows*cols)                           │
│               with per-cell instance data:               │
│               [px, py, cell_w, cell_h, uv_u0, uv_v0,     │
│                uv_u1, uv_v1]                             │
│                                                          │
│  Vertex shader: a_position * a_size + a_offset → clip   │
│  Fragment shader: texture2D(atlas, v_texcoord) * color   │
└──────────────────────────────────────────────────────────┘
```

### Files

| File | Purpose |
|---|---|
| `client-wasm/src/renderer.rs` | WebGL2 glyph atlas renderer implementation |
| `client-wasm/src/lib.rs` | Public API (integrates renderer with WASM exports) |
| `client-wasm/Cargo.toml` | `ab_glyph = "0.2"` dependency |

### Key Types in `renderer.rs`

- **`GlyphAtlas`**: WebGL2 texture + UV map for all printable ASCII chars. Methods: `new()`, `rebuild()`, `uv_for()`
- **`GlyphBrush`**: Compiled shader program + vertex buffers + uniform locations. Methods: `new()`, `compile_program()`, `compile_shader()`
- **`WebGL2Renderer`**: Main renderer. Methods: `new()`, `rebuild_atlas()`, `render()`, `build_instances()`

### Glyph Atlas Layout

All glyphs are rendered in a single texture with row-major packing (16 columns):

```
 [A][B][C]...[P]
 [Q][R][S]...[Z]
 [a][b][c]...[p]
 [q][r][s]...[z]
 [0][1][2]...[9]
 [!]["][#]...[/]
 [:][;][<]...[~]
```

Each cell is padded by `ATLAS_PADDING = 2` pixels to prevent filtering bleed between adjacent glyphs.

### Zoom Handling

When the user zooms in Firefox (DPR changes), `handle_resize()` recalculates cell dimensions and calls `WebGL2Renderer::rebuild_atlas()`. This re-rasterizes all 95 glyphs at the new DPR and uploads a new texture. The quads stay at CSS pixel dimensions; WebGL2 handles the scaling via nearest-neighbor filtering.

### Performance Characteristics

- **Startup**: ~10ms to rasterize and upload 95 glyphs (one-time)
- **Per-frame render**: ~0.1ms for 80×24 grid (one instanced draw call)
- **Atlas rebuild on zoom**: ~5ms to re-rasterize (only when cell size changes)
- **Memory**: ~95 × glyph_w × glyph_h bytes for the atlas texture

### Dependencies Added

- `ab_glyph = "0.2"` — Font loading and outline rasterization

### Integration Plan

1. **Replace `TerminalState::render()`** — Call `WebGL2Renderer::render()` instead of Canvas 2D drawing
2. **Embed font binary** — Use `include_bytes!("path/to/DejaVuSansMono.ttf")` in `init()`
3. **Update `init()`** — Create `WebGL2Renderer` instead of `TerminalState`
4. **Update `process_bytes()`** — Call renderer instead of Canvas 2D rendering
5. **Keep `TerminalState` as fallback** — If WebGL2 context unavailable, fall back to Canvas 2D

### Shader Code

**Vertex shader** (`VERTEX_SHADER`):
```glsl
attribute vec2 a_position;
attribute vec2 a_texcoord;
attribute vec2 a_offset;
attribute float a_size;
varying vec2 v_texcoord;
uniform vec2 u_resolution;
void main() {
    vec2 scaled = a_position * a_size + a_offset;
    gl_Position = vec4(scaled / u_resolution * 2.0 - 1.0, 0.0, 1.0);
    v_texcoord = a_texcoord;
}
```

**Fragment shader** (`FRAGMENT_SHADER`):
```glsl
precision mediump float;
varying vec2 v_texcoord;
uniform sampler2D u_atlas;
uniform vec4 u_color;
void main() {
    float alpha = texture2D(u_atlas, v_texcoord).a;
    gl_FragColor = vec4(u_color.rgb, u_color.a * alpha);
}
```

### Instance Data Format

Each instance (one cell) has 8 floats:
```
[px, py, cell_w, cell_h, uv_u0, uv_v0, uv_u1, uv_v1]
```
- `px, py`: pixel position on canvas
- `cell_w, cell_h`: cell dimensions in pixels
- `uv_u0, uv_v0, uv_u1, uv_v1`: texture coordinates in the atlas

The vertex shader scales `a_position` (0..1 quad) by `a_size` (cell dimensions) and offsets by `a_offset` (pixel position).
