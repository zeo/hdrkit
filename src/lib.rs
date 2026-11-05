//! Pixel shaders for HDR screen capture on Windows: read a monitor's SDR white and peak, tonemap
//! scRGB frames to sRGB, write scRGB or sRGB frames as HDR10 P010, and resample with kernels that
//! widen with the downscale ratio.

/// Every D3D11 type in this crate's API comes from this version of `windows`. Callers on another
/// version can pass interfaces across with `Interface::into_raw` and `from_raw`.
pub use windows;
use windows::core::{s, Error, Interface, Result, PCSTR};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{E_FAIL, LUID, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SHADER_MACRO};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;

/// scRGB 1.0 is defined as 80 nits
pub const SCRGB_WHITE: f32 = 80.0;

/// A monitor's brightness setup. The tonemap uses the fixed panel peak, so it never pumps with content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub hdr: bool,
    /// nits that windows maps sdr white to, from the "sdr content brightness" slider
    pub sdr_white: f32,
    /// panel peak in nits as the driver reports it
    pub peak: f32,
}

/// Fails with `DXGI_ERROR_NOT_FOUND` when no adapter drives `monitor`. SDR white falls back to
/// 200 nits when the display config query fails, the panel peak to 1000 when the driver reports 0.
pub fn display(monitor: HMONITOR) -> Result<Display> {
    let desc = output(monitor)?;
    let hdr = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    let sdr_white = if hdr { sdr_white(&desc.DeviceName).unwrap_or(200.0) } else { SCRGB_WHITE };
    let peak = if desc.MaxLuminance > 0.0 { desc.MaxLuminance } else { 1000.0 };
    Ok(Display { hdr, sdr_white, peak: peak.max(sdr_white) })
}

/// What an hdr10 file states about the screen it was made on: the panel's own primaries, white
/// point and luminance range, with that peak standing in for the content light level too.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mastering {
    /// red, green, blue, then the white point, as CIE 1931 xy
    pub primaries: [[f32; 2]; 4],
    pub min: f32,
    pub max: f32,
    /// brightest frame average the panel holds, which stands in for MaxFALL
    pub average: f32,
}

/// DCI-P3 with a D65 white, what HDR10 is usually mastered on, for drivers that report no primaries
const P3_D65: [[f32; 2]; 4] = [[0.68, 0.32], [0.265, 0.69], [0.15, 0.06], [0.3127, 0.329]];

pub fn mastering(monitor: HMONITOR) -> Result<Mastering> {
    let desc = output(monitor)?;
    let reported = [desc.RedPrimary, desc.GreenPrimary, desc.BluePrimary, desc.WhitePoint];
    let primaries = if reported.iter().flatten().all(|&v| v > 0.0 && v < 1.0) { reported } else { P3_D65 };
    let max = if desc.MaxLuminance > 0.0 { desc.MaxLuminance } else { 1000.0 };
    let average = if desc.MaxFullFrameLuminance > 0.0 { desc.MaxFullFrameLuminance.min(max) } else { max };
    Ok(Mastering { primaries, min: desc.MinLuminance.clamp(0.0, max), max, average })
}

fn output(monitor: HMONITOR) -> Result<DXGI_OUTPUT_DESC1> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let desc = output.cast::<IDXGIOutput6>()?.GetDesc1()?;
                if desc.Monitor == monitor {
                    return Ok(desc);
                }
                o += 1;
            }
            a += 1;
        }
    }
    Err(Error::new(DXGI_ERROR_NOT_FOUND, "the monitor is not on any adapter"))
}

fn sdr_white(device: &[u16; 32]) -> Option<f32> {
    let name = |s: &[u16]| s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())].to_vec();
    let header = |kind, size: usize, adapter: LUID, id| DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size as u32,
        adapterId: adapter,
        id,
    };
    unsafe {
        let (mut path_count, mut mode_count) = (0, 0);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).ok().ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .ok()
        .ok()?;
        paths.truncate(path_count as usize);
        paths.iter().find_map(|path| {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: header(
                    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(),
                    path.sourceInfo.adapterId,
                    path.sourceInfo.id,
                ),
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 || name(&source.viewGdiDeviceName) != name(device) {
                return None;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: header(
                    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(),
                    path.targetInfo.adapterId,
                    path.targetInfo.id,
                ),
                ..Default::default()
            };
            // the level is in thousandths of scRGB white
            let nits = white_level(DisplayConfigGetDeviceInfo(&mut white.header), white.SDRWhiteLevel);
            nits.filter(|n| (SCRGB_WHITE..=10_000.0).contains(n))
        })
    }
}

fn white_level(status: i32, level: u32) -> Option<f32> {
    (status == 0 && level > 0).then(|| level as f32 * SCRGB_WHITE / 1000.0)
}

const VERTEX: &str = r#"
struct Vertex {
    float4 position : SV_Position;
    float2 uv : TEXCOORD;
};

// one triangle covering the viewport, uv runs 0 to 1 across it
Vertex vs(uint id : SV_VertexID) {
    Vertex v;
    v.uv = float2((id << 1) & 2, id & 2);
    v.position = float4(v.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return v;
}
"#;

const TONEMAP: &str = r#"
Texture2D<float4> frame : register(t0);
cbuffer Curve : register(b0) {
    // scRGB to sdr-relative linear light
    float scale;
    float knee;
    float soft;
};

float4 sdr(float4 position : SV_Position) : SV_Target {
    float3 c = max(frame.Load(int3(position.xy, 0)).rgb * scale, 0);
    float m = max(c.r, max(c.g, c.b));
    // one factor for all channels keeps the hue of bright colors
    if (m > knee) {
        float e = m - knee;
        c *= (knee + e / (1 + e / soft)) / m;
    }
    c = saturate(c);
    return float4(c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1 / 2.4) - 0.055, 1);
}
"#;

const PQ: &str = r#"
Texture2D<float4> frame : register(t0);
SamplerState bilinear : register(s0);
cbuffer Crop : register(b0) {
    float2 scale;
    float shift;
    // nits for sdr white when the frame holds sRGB values, 0 when it holds scRGB
    float white;
};

static const float3x3 BT709_TO_BT2020 = {
    0.627404, 0.329283, 0.043313,
    0.069097, 0.919540, 0.011362,
    0.016391, 0.088013, 0.895595,
};

float3 pq(float2 uv) {
    float3 c = frame.SampleLevel(bilinear, uv * scale, 0).rgb;
    if (white > 0) {
        c = saturate(c);
        c = (c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)) * white / 80;
    }
    float3 nits = mul(BT709_TO_BT2020, c) * 80;
    float3 y = pow(saturate(nits / 10000), 0.1593017578125);
    return pow((0.8359375 + 18.8515625 * y) / (1 + 18.6875 * y), 78.84375);
}

// p010 keeps limited range 10 bit codes in the top bits of each 16 bit sample
float code(float v) {
    return round(v) * 64 / 65535;
}

float bt2020_luma(float3 c) {
    return dot(c, float3(0.2627, 0.6780, 0.0593));
}

float luma(float4 position : SV_Position, float2 uv : TEXCOORD) : SV_Target {
    return code(64 + 876 * bt2020_luma(pq(uv)));
}

// left chroma siting: in line with the left luma column, halfway between the two rows
float2 chroma(float4 position : SV_Position, float2 uv : TEXCOORD) : SV_Target {
    float3 c = pq(uv - float2(shift, 0));
    float y = bt2020_luma(c);
    return float2(code(512 + 896 * (c.b - y) / 1.8814), code(512 + 896 * (c.r - y) / 1.4746));
}
"#;

const RESAMPLE: &str = r#"
Texture2D<float4> frame : register(t0);
cbuffer Resample : register(b0) {
    // render target position of the output's top left corner
    float2 offset;
    // source texels per output pixel on each axis
    float2 scale;
    // the axis this pass filters, the other passes straight through
    float2 axis;
    // source texels that hold content on each axis
    float2 limit;
};

static const float PI = 3.14159265;

// window half width in source texels when each output pixel covers k of them
float radius(float k) {
    if (FILTER == 0) return 1;
    if (FILTER == 1) return k / 2 + 0.5;
    if (FILTER == 2) return 2 * k;
    return 3 * k;
}

// weight of the texel whose center sits d source texels from the sample point
float weight(float d, float k) {
    if (FILTER == 0) return max(0, 1 - abs(d));
    // how much of the texel falls inside the output pixel's footprint
    if (FILTER == 1) return max(0, min(d + 0.5, k / 2) - max(d - 0.5, -k / 2));
    float x = abs(d) / k;
    // catmull-rom
    if (FILTER == 2) return x < 1 ? (1.5 * x - 2.5) * x * x + 1 : x < 2 ? ((-0.5 * x + 2.5) * x - 4) * x + 2 : 0;
    if (x < 1e-4) return 1;
    return x < 3 ? 3 * sin(PI * x) * sin(PI * x / 3) / (PI * PI * x * x) : 0;
}

float4 resample(float4 position : SV_Position) : SV_Target {
    float2 s = (position.xy - offset) * scale;
    float c = dot(s, axis);
    // a kernel never gets narrower than a source texel, so upscaling stays smooth
    float k = max(dot(scale, axis), 1);
    float r = radius(k);
    int2 across = int2(floor(s * (1 - axis)));
    int last = int(dot(limit, axis)) - 1;
    float4 sum = 0;
    float total = 0;
    [loop] for (int i = int(floor(c - r)); i <= int(ceil(c + r)); i++) {
        float w = weight(i + 0.5 - c, k);
        sum += w * frame.Load(int3(across + int2(axis) * clamp(i, 0, last), 0));
        total += w;
    }
    return sum / total;
}
"#;

/// How frames are resampled to the encode size. Every kernel but bilinear widens with the
/// downscale ratio, so detail averages out instead of aliasing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Bilinear,
    Area,
    Bicubic,
    Lanczos,
}

impl Filter {
    pub fn named(name: &str) -> Option<Self> {
        [Filter::Bilinear, Filter::Area, Filter::Bicubic, Filter::Lanczos]
            .into_iter()
            .find(|f| format!("{f:?}").eq_ignore_ascii_case(name))
    }
}

/// Resamples the content of a frame into a rect of a fixed size target, black around it, in two
/// separable passes through a half way texture.
pub struct Scaler {
    program: Program,
    pass: ID3D11Buffer,
    output: (ID3D11Texture2D, ID3D11RenderTargetView),
    middle: Option<(ID3D11Texture2D, ID3D11RenderTargetView, (u32, u32))>,
}

impl Scaler {
    pub fn new(device: &ID3D11Device, filter: Filter, size: (u32, u32), format: DXGI_FORMAT) -> Result<Self> {
        let program = Program::new(device, RESAMPLE, &[s!("resample")], &[(s!("FILTER"), filter as u8 as f32)])?;
        let texture = texture(device, size, format)?;
        let view = view(device, &texture, format)?;
        Ok(Self { program, pass: constants(device, 32)?, output: (texture, view), middle: None })
    }

    /// Scales the `content` region of `source` into `dest` of the output and returns the output,
    /// which the next call draws over.
    pub fn apply(&mut self, source: &ID3D11Texture2D, content: (u32, u32), dest: RECT) -> Result<ID3D11Texture2D> {
        let size = size(source);
        let content = (content.0.clamp(1, size.0), content.1.clamp(1, size.1));
        let extent = ((dest.right - dest.left).max(1) as u32, (dest.bottom - dest.top).max(1) as u32);
        let between = (extent.0, content.1);
        if self.middle.as_ref().is_none_or(|m| m.2 != between) {
            let texture = texture(&self.program.device, between, DXGI_FORMAT_R16G16B16A16_FLOAT)?;
            let view = view(&self.program.device, &texture, DXGI_FORMAT_R16G16B16A16_FLOAT)?;
            self.middle = Some((texture, view, between));
        }
        let (middle, middle_view, _) = self.middle.as_ref().expect("created");
        let limit = [content.0 as f32, content.1 as f32];
        let across = [content.0 as f32 / extent.0 as f32, 1.0, 1.0, 0.0];
        let down = [1.0, content.1 as f32 / extent.1 as f32, 0.0, 1.0];
        let bind = |c: &ID3D11DeviceContext, offset: [f32; 2], scale_axis: [f32; 4]| {
            let values =
                [offset[0], offset[1], scale_axis[0], scale_axis[1], scale_axis[2], scale_axis[3], limit[0], limit[1]];
            upload(c, &self.pass, &values);
        };
        self.program.draw(source, |c| bind(c, [0.0, 0.0], across), &[(0, middle_view, viewport(0, 0, between))])?;
        let (output, view) = &self.output;
        let clear = |c: &ID3D11DeviceContext| unsafe {
            c.ClearRenderTargetView(view, &[0.0, 0.0, 0.0, 1.0]);
            bind(c, [dest.left as f32, dest.top as f32], down);
        };
        self.program.draw(middle, clear, &[(0, view, viewport(dest.left, dest.top, extent))])?;
        Ok(output.clone())
    }
}

/// Full viewport pixel shader passes from one texture, compiled at startup with the
/// d3dcompiler that ships with windows.
struct Program {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixels: Vec<ID3D11PixelShader>,
}

impl Program {
    fn new(device: &ID3D11Device, source: &str, entries: &[PCSTR], defines: &[(PCSTR, f32)]) -> Result<Self> {
        let source = [VERTEX, source].concat();
        let values: Vec<String> = defines.iter().map(|(_, v)| format!("{v:e}\0")).collect();
        let mut macros: Vec<D3D_SHADER_MACRO> = defines
            .iter()
            .zip(&values)
            .map(|((name, _), value)| D3D_SHADER_MACRO { Name: *name, Definition: PCSTR(value.as_ptr()) })
            .collect();
        macros.push(D3D_SHADER_MACRO::default());
        unsafe {
            let vertex = compile(&source, s!("vs"), s!("vs_5_0"), &macros)?;
            let mut vs = None;
            device.CreateVertexShader(bytes(&vertex), None, Some(&mut vs))?;
            let mut pixels = Vec::with_capacity(entries.len());
            for &entry in entries {
                let pixel = compile(&source, entry, s!("ps_5_0"), &macros)?;
                let mut ps = None;
                device.CreatePixelShader(bytes(&pixel), None, Some(&mut ps))?;
                pixels.push(ps.ok_or(Error::from(E_FAIL))?);
            }
            Ok(Self {
                device: device.clone(),
                context: device.GetImmediateContext()?,
                vertex: vs.ok_or(Error::from(E_FAIL))?,
                pixels,
            })
        }
    }

    /// Runs pixel shader `i` into each `(i, target, viewport)` with `source` bound to t0.
    fn draw(
        &self,
        source: &ID3D11Texture2D,
        setup: impl FnOnce(&ID3D11DeviceContext),
        passes: &[(usize, &ID3D11RenderTargetView, D3D11_VIEWPORT)],
    ) -> Result<()> {
        unsafe {
            let mut input = None;
            self.device.CreateShaderResourceView(source, None, Some(&mut input))?;
            // encoders behind an IMFDXGIDeviceManager use the immediate context from their own threads
            let lock: ID3D11Multithread = self.context.cast()?;
            lock.Enter();
            let c = &self.context;
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.IASetInputLayout(None);
            c.VSSetShader(&self.vertex, None);
            c.PSSetShaderResources(0, Some(&[input]));
            c.RSSetState(None);
            c.OMSetBlendState(None, None, u32::MAX);
            setup(c);
            for &(i, target, viewport) in passes {
                c.PSSetShader(&self.pixels[i], None);
                c.RSSetViewports(Some(&[viewport]));
                c.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
                c.Draw(3, 0);
            }
            c.OMSetRenderTargets(None, None);
            c.PSSetShaderResources(0, Some(&[None]));
            lock.Leave();
            Ok(())
        }
    }
}

/// Turns scRGB FP16 frames into sRGB BGRA on the GPU: sdr content keeps its normal brightness
/// and highlights roll off above a knee instead of clipping. The curve comes from the display,
/// not the frame, so it stays the same from frame to frame until the display changes.
pub struct Tonemap {
    program: Program,
    curve: ID3D11Buffer,
    values: [f32; 4],
    target: Option<(ID3D11Texture2D, ID3D11RenderTargetView, (u32, u32))>,
}

/// Linear sdr-relative level below which sdr content passes through untouched, about sRGB 231
const KNEE: f32 = 0.8;

impl Tonemap {
    pub fn new(device: &ID3D11Device, display: Display) -> Result<Self> {
        let program = Program::new(device, TONEMAP, &[s!("sdr")], &[])?;
        Ok(Self { program, curve: constants(device, 16)?, values: curve(display), target: None })
    }

    /// Takes on a new sdr white or peak from the next frame on, without recompiling anything.
    pub fn follow(&mut self, display: Display) {
        self.values = curve(display);
    }

    /// Returns a BGRA texture the size of `source`, which the next call draws over.
    pub fn apply(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let size = size(source);
        if self.target.as_ref().is_none_or(|t| t.2 != size) {
            let texture = texture(&self.program.device, size, DXGI_FORMAT_B8G8R8A8_UNORM)?;
            let view = view(&self.program.device, &texture, DXGI_FORMAT_B8G8R8A8_UNORM)?;
            self.target = Some((texture, view, size));
        }
        let (texture, view, _) = self.target.as_ref().expect("created");
        self.program.draw(source, |c| upload(c, &self.curve, &self.values), &[(0, view, viewport(0, 0, size))])?;
        Ok(texture.clone())
    }
}

/// Writes scRGB FP16 frames, or sRGB ones at a given sdr white, straight into P010 as hdr10: bt.2020
/// primaries, the PQ curve in absolute nits, limited range. Video processors are free to refuse PQ
/// output (NVIDIA's does), so the color math and the scaling happen here. `count` targets rotate, so
/// an encoder can still hold the last few while the next is drawn.
pub struct Pq {
    program: Program,
    sampler: ID3D11SamplerState,
    crop: ID3D11Buffer,
    targets: Vec<(ID3D11Texture2D, ID3D11RenderTargetView, ID3D11RenderTargetView)>,
    next: usize,
}

impl Pq {
    pub fn new(device: &ID3D11Device, output: (u32, u32), count: usize) -> Result<Self> {
        let program = Program::new(device, PQ, &[s!("luma"), s!("chroma")], &[])?;
        unsafe {
            let mut sampler = None;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            let mut targets = Vec::with_capacity(count);
            for _ in 0..count {
                let texture = texture(device, output, DXGI_FORMAT_P010)?;
                // the view format picks the plane: 16 bit luma, then 16 bit pairs of half size chroma
                let luma = view(device, &texture, DXGI_FORMAT_R16_UNORM)?;
                let chroma = view(device, &texture, DXGI_FORMAT_R16G16_UNORM)?;
                targets.push((texture, luma, chroma));
            }
            Ok(Self {
                program,
                sampler: sampler.ok_or(Error::from(E_FAIL))?,
                crop: constants(device, 16)?,
                targets,
                next: 0,
            })
        }
    }

    /// Scales the `content` region of `source` into `dest` (even pixels) of the next target, black around it.
    /// `sdr_white` marks the source as sRGB and places its white at that many nits.
    pub fn convert(
        &mut self,
        source: &ID3D11Texture2D,
        content: (u32, u32),
        dest: RECT,
        sdr_white: Option<f32>,
    ) -> Result<ID3D11Texture2D> {
        let size = size(source);
        let content = (content.0.clamp(1, size.0), content.1.clamp(1, size.1));
        let width = (dest.right - dest.left).max(2) as f32;
        let crop =
            [content.0 as f32 / size.0 as f32, content.1 as f32 / size.1 as f32, 0.5 / width, sdr_white.unwrap_or(0.0)];
        let (texture, luma, chroma) = &self.targets[self.next];
        self.next = (self.next + 1) % self.targets.len();
        let at = |divisor: i32| {
            let corner = (dest.left / divisor, dest.top / divisor);
            let extent = ((dest.right - dest.left) / divisor, (dest.bottom - dest.top) / divisor);
            viewport(corner.0, corner.1, (extent.0 as u32, extent.1 as u32))
        };
        let setup = |c: &ID3D11DeviceContext| unsafe {
            let black = 64.0 * 64.0 / 65535.0;
            let neutral = 512.0 * 64.0 / 65535.0;
            c.ClearRenderTargetView(luma, &[black, 0.0, 0.0, 0.0]);
            c.ClearRenderTargetView(chroma, &[neutral, neutral, 0.0, 0.0]);
            upload(c, &self.crop, &crop);
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        };
        self.program.draw(source, setup, &[(0, luma, at(1)), (1, chroma, at(2))])?;
        Ok(texture.clone())
    }
}

fn size(texture: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

fn viewport(left: i32, top: i32, size: (u32, u32)) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT {
        TopLeftX: left as f32,
        TopLeftY: top as f32,
        Width: size.0 as f32,
        Height: size.1 as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

fn texture(device: &ID3D11Device, size: (u32, u32), format: DXGI_FORMAT) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or(Error::from(E_FAIL))
}

/// A pixel shader constant buffer of `bytes`, a multiple of 16
fn constants(device: &ID3D11Device, bytes: u32) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: bytes,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer))? };
    buffer.ok_or(Error::from(E_FAIL))
}

fn upload(c: &ID3D11DeviceContext, buffer: &ID3D11Buffer, values: &[f32]) {
    unsafe {
        c.UpdateSubresource(buffer, 0, None, values.as_ptr() as _, 0, 0);
        c.PSSetConstantBuffers(0, Some(&[Some(buffer.clone())]));
    }
}

fn view(device: &ID3D11Device, texture: &ID3D11Texture2D, format: DXGI_FORMAT) -> Result<ID3D11RenderTargetView> {
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
    };
    let mut view = None;
    unsafe { device.CreateRenderTargetView(texture, Some(&desc), Some(&mut view))? };
    view.ok_or(Error::from(E_FAIL))
}

unsafe fn compile(source: &str, entry: PCSTR, target: PCSTR, macros: &[D3D_SHADER_MACRO]) -> Result<ID3DBlob> {
    let (mut code, mut errors) = (None, None);
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr() as _,
            source.len(),
            None,
            Some(macros.as_ptr()),
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        let log = errors.map(|b| String::from_utf8_lossy(bytes(&b)).into_owned()).unwrap_or_default();
        return Err(Error::new(e.code(), format!("shader didn't compile: {log}")));
    }
    code.ok_or(Error::from(E_FAIL))
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

/// Knee and softness of the highlight curve in sdr-relative linear light, where sdr white is 1.0.
/// Above the knee `e / (1 + e / soft)` bends the panel peak exactly onto white without a kink.
/// A panel no brighter than sdr white has nothing to roll off, so the knee moves out of reach.
fn curve(display: Display) -> [f32; 4] {
    let (knee, soft) = rolloff(display);
    [SCRGB_WHITE / display.sdr_white, knee, soft, 0.0]
}

fn rolloff(display: Display) -> (f32, f32) {
    let peak = display.peak / display.sdr_white;
    let (range, room) = (peak - KNEE, 1.0 - KNEE);
    if range <= room {
        return (1e30, 1.0);
    }
    (KNEE, range * room / (range - room))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_level_is_thousandths_of_scrgb_white() {
        assert_eq!(white_level(0, 2500), Some(200.0));
        assert_eq!(white_level(0, 0), None);
        assert_eq!(white_level(87, 2500), None);
    }

    #[test]
    fn rolloff_lands_the_peak_on_white() {
        let (knee, soft) = rolloff(Display { hdr: true, sdr_white: 200.0, peak: 1000.0 });
        let excess = 1000.0 / 200.0 - knee;
        assert!((knee + excess / (1.0 + excess / soft) - 1.0).abs() < 1e-6);
        let (knee, _) = rolloff(Display { hdr: true, sdr_white: 400.0, peak: 300.0 });
        assert!(knee > 1e20);
    }
}
