# hdrkit

D3D11 pixel shaders and display queries for capturing an HDR Windows desktop. It's the color code shared by [capscr](https://github.com/zeo/capscr) and memory, a game clipper.

With HDR on, the compositor hands you scRGB FP16: linear light where 1.0 is 80 nits and SDR white sits wherever the "SDR content brightness" slider put it. At 240 nits that's 3.0. Clip it straight to 8-bit and every SDR window comes out blown out. hdrkit reads the slider and the panel peak for one monitor, then turns frames into something an encoder or a PNG can take.

- `display(monitor)` gives `Display { hdr, sdr_white, peak }`. HDR means the output's color space is `RGB_FULL_G2084_NONE_P2020`. SDR white comes from `DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL`, and the peak is the driver's `MaxLuminance`.
- `Tonemap` maps scRGB to sRGB `B8G8R8A8_UNORM`. SDR content up to sRGB 231 (linear 0.8) comes back at its own 8-bit value. Above that, one gain per pixel bends the panel peak onto white, which keeps the hue of bright colors. Parameters come from the display and not the frame, so brightness never pumps between frames.
- `Pq` writes HDR10 into `P010`: BT.2020 primaries, the PQ curve in absolute nits, limited range, left chroma siting. Input is scRGB, or 8-bit sRGB placed at a white level you choose (BT.2408's reference white is 203 nits). It exists because the D3D11 video processor is allowed to refuse PQ output, and NVIDIA's does.
- `mastering(monitor)` gives the panel's primaries and luminance range for an HDR10 file's `mdcv` and `clli` boxes. If the driver's primaries are zero or out of range, it falls back to P3 D65.
- `Scaler` resamples into a letterboxed target with bilinear, area, Catmull-Rom bicubic or Lanczos-3. It runs as two separable passes through an FP16 texture. Every kernel except bilinear widens with the downscale ratio, so a 4:1 shrink averages detail where a fixed kernel would alias it.

Shaders compile when each object is built, with the `d3dcompiler_47.dll` that ships with Windows.

## use

```toml
[dependencies]
hdrkit = { git = "https://github.com/zeo/hdrkit", rev = "<commit>" }
```

```rust
let display = hdrkit::display(monitor)?;
let mut tonemap = hdrkit::Tonemap::new(&device, display)?;
let sdr = tonemap.apply(&frame)?; // B8G8R8A8_UNORM, the size of frame, reused by the next call
```

Build a new `Tonemap` when `display` returns something different. The values are baked into the shader.

The API uses `windows` 0.62 types, and the crate re-exports that version as `hdrkit::windows`. Code on another `windows` version can share one device by passing it across with `Interface::into_raw` and `from_raw`. Or it can do its capture entirely in `hdrkit::windows` types, like capscr's Desktop Duplication path.

## test

```
cargo test
```

The shader tests run on WARP, so they need no GPU and pass on a headless CI runner. They check that SDR grey 128 survives the tonemap and the panel peak reaches 255. They compare the PQ codes at 203 and 1000 nits against a separate ST 2084 implementation, require every filter to leave a frame untouched at 1:1, and require area to average one white column in four to a flat 64.

## limits

- Only an NVIDIA GPU and WARP have run it so far. AMD and Intel drivers are untested.
- Mastering values are whatever the driver reports. Some panels report BT.709 primaries, or a full-frame luminance equal to their peak.
- The tonemap clips wide-gamut colors channel by channel. It doesn't map them into the sRGB gamut.

## license

MIT or Apache-2.0, at your option.
