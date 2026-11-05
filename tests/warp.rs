//! The shaders run on WARP, the software rasterizer every windows install ships, so these need no GPU.

use hdrkit::windows::Win32::Foundation::{HMODULE, RECT};
use hdrkit::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
use hdrkit::windows::Win32::Graphics::Direct3D11::*;
use hdrkit::windows::Win32::Graphics::Dxgi::Common::*;
use hdrkit::{Display, Filter, Pq, Scaler, Tonemap, SCRGB_WHITE};

const PANEL: Display = Display { hdr: true, sdr_white: 200.0, peak: 1000.0 };

fn warp() -> (ID3D11Device, ID3D11DeviceContext) {
    let (mut device, mut context) = (None, None);
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .unwrap();
    }
    (device.unwrap(), context.unwrap())
}

fn upload(device: &ID3D11Device, size: (u32, u32), format: DXGI_FORMAT, bytes: &[u8]) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr() as _,
        SysMemPitch: bytes.len() as u32 / size.1,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)).unwrap() };
    texture.unwrap()
}

/// Rows of `texture` on the CPU, both planes for P010, and the row pitch.
fn read((device, context): &(ID3D11Device, ID3D11DeviceContext), texture: &ID3D11Texture2D) -> (Vec<u8>, usize) {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        let rows = if desc.Format == DXGI_FORMAT_P010 { desc.Height * 3 / 2 } else { desc.Height } as usize;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap();
        let staging = staging.unwrap();
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
        let pitch = mapped.RowPitch as usize;
        let bytes = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * rows).to_vec();
        context.Unmap(&staging, 0);
        (bytes, pitch)
    }
}

/// A 2x2 scRGB texture of one grey level, given in nits.
fn scrgb(device: &ID3D11Device, nits: f32) -> ID3D11Texture2D {
    let v = half(nits / SCRGB_WHITE);
    let pixel = [v, v, v, half(1.0)].map(u16::to_le_bytes).concat();
    upload(device, (2, 2), DXGI_FORMAT_R16G16B16A16_FLOAT, &pixel.repeat(4))
}

/// f32 to IEEE half, for positive normal values
fn half(value: f32) -> u16 {
    let bits = value.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    ((exponent.clamp(1, 30) as u16) << 10) | ((bits >> 13) & 0x3ff) as u16
}

fn srgb_to_linear(code: u8) -> f32 {
    let v = code as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// SMPTE ST 2084, written apart from the shader so the two can disagree
fn pq_code(nits: f32) -> u32 {
    let y = (nits / 10_000.0).powf(0.159_301_76);
    let e = ((0.835_937_5 + 18.851_563 * y) / (1.0 + 18.6875 * y)).powf(78.84375);
    (64.0 + 876.0 * e).round() as u32
}

#[test]
fn tonemap_keeps_sdr_grey_and_puts_the_panel_peak_on_white() {
    let gpu = warp();
    let mut tonemap = Tonemap::new(&gpu.0, PANEL).unwrap();
    let grey = srgb_to_linear(128) * PANEL.sdr_white;
    let mut level = |nits: f32| read(&gpu, &tonemap.apply(&scrgb(&gpu.0, nits)).unwrap()).0[1];
    let sdr = level(grey);
    assert!(sdr.abs_diff(128) <= 1, "grey came out as {sdr}");
    assert_eq!(level(PANEL.peak), 255);
    let highlight = level(PANEL.sdr_white * 1.25);
    assert!((236..255).contains(&highlight), "a highlight above sdr white came out as {highlight}");
}

#[test]
fn a_followed_display_matches_a_tonemap_built_for_it() {
    let gpu = warp();
    let dimmer = Display { sdr_white: 120.0, peak: 600.0, ..PANEL };
    let mut followed = Tonemap::new(&gpu.0, PANEL).unwrap();
    let mut fresh = Tonemap::new(&gpu.0, dimmer).unwrap();
    let before = read(&gpu, &followed.apply(&scrgb(&gpu.0, 150.0)).unwrap()).0[1];
    followed.follow(dimmer);
    for nits in [60.0, 150.0, 400.0] {
        let frame = scrgb(&gpu.0, nits);
        let (a, b) =
            (read(&gpu, &followed.apply(&frame).unwrap()).0[1], read(&gpu, &fresh.apply(&frame).unwrap()).0[1]);
        assert_eq!(a, b, "{nits} nits");
    }
    assert_ne!(before, read(&gpu, &followed.apply(&scrgb(&gpu.0, 150.0)).unwrap()).0[1]);
}

#[test]
fn pq_writes_st_2084_codes_with_neutral_chroma() {
    let gpu = warp();
    let mut pq = Pq::new(&gpu.0, (2, 2), 1).unwrap();
    let full = RECT { left: 0, top: 0, right: 2, bottom: 2 };
    for nits in [203.0, 1000.0] {
        let (p010, pitch) = read(&gpu, &pq.convert(&scrgb(&gpu.0, nits), (2, 2), full, None).unwrap());
        let sample = |at: usize| u16::from_le_bytes([p010[at], p010[at + 1]]) as u32 >> 6;
        assert!(sample(0).abs_diff(pq_code(nits)) <= 1, "{nits} nits gave luma {}", sample(0));
        assert!(sample(2 * pitch).abs_diff(512) <= 1 && sample(2 * pitch + 2).abs_diff(512) <= 1);
    }
}

#[test]
fn pq_places_srgb_white_at_the_given_nits() {
    let gpu = warp();
    let mut pq = Pq::new(&gpu.0, (2, 2), 1).unwrap();
    let full = RECT { left: 0, top: 0, right: 2, bottom: 2 };
    let white = upload(&gpu.0, (2, 2), DXGI_FORMAT_B8G8R8A8_UNORM, &[255; 16]);
    let (p010, _) = read(&gpu, &pq.convert(&white, (2, 2), full, Some(203.0)).unwrap());
    let luma = u16::from_le_bytes([p010[0], p010[1]]) as u32 >> 6;
    assert!(luma.abs_diff(pq_code(203.0)) <= 1, "luma {luma}");
}

#[test]
fn every_filter_is_identity_at_one_to_one() {
    let gpu = warp();
    let gradient: Vec<u8> = (0..=255u8).flat_map(|i| [i, 255 - i, i / 2, 255]).collect();
    let source = upload(&gpu.0, (16, 16), DXGI_FORMAT_B8G8R8A8_UNORM, &gradient);
    let (original, pitch) = read(&gpu, &source);
    for filter in [Filter::Bilinear, Filter::Area, Filter::Bicubic, Filter::Lanczos] {
        let mut scaler = Scaler::new(&gpu.0, filter, (16, 16), DXGI_FORMAT_B8G8R8A8_UNORM).unwrap();
        let full = RECT { left: 0, top: 0, right: 16, bottom: 16 };
        let (copy, _) = read(&gpu, &scaler.apply(&source, (16, 16), full).unwrap());
        let rows = |b: &[u8]| (0..16).flat_map(|y| b[y * pitch..y * pitch + 64].to_vec()).collect::<Vec<_>>();
        let off = rows(&original).iter().zip(rows(&copy)).map(|(a, b)| a.abs_diff(b)).max();
        assert!(off <= Some(1), "{filter:?} moved a value by {off:?}");
    }
}

#[test]
fn area_averages_one_white_column_in_four_to_a_flat_quarter() {
    let gpu = warp();
    let stripes: Vec<u8> = (0..64 * 16).flat_map(|i| if i % 4 == 0 { [255; 4] } else { [0, 0, 0, 255] }).collect();
    let source = upload(&gpu.0, (64, 16), DXGI_FORMAT_B8G8R8A8_UNORM, &stripes);
    let mut scaler = Scaler::new(&gpu.0, Filter::Area, (16, 4), DXGI_FORMAT_B8G8R8A8_UNORM).unwrap();
    let quarter = RECT { left: 0, top: 0, right: 16, bottom: 4 };
    let (bytes, pitch) = read(&gpu, &scaler.apply(&source, (64, 16), quarter).unwrap());
    let blue: Vec<u8> = (0..4).flat_map(|y| (0..16).map(move |x| y * pitch + x * 4)).map(|i| bytes[i]).collect();
    assert!(blue.iter().all(|&b| (63..=65).contains(&b)), "{blue:?}");
}
