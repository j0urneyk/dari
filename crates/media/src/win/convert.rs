//! Scaling captured BGRA frames into NV12 on the GPU.
//!
//! Two pixel shader passes render straight into the planes of an NV12 texture: luma at full
//! size, then chroma at half size. They use OpenH264's own BT.601 limited-range coefficients and
//! truncation, and average each 2×2 block for chroma as it does, so frames converted here match
//! frames OpenH264 converts on the CPU. Downscaling averages four bilinear taps spread over each
//! output pixel's footprint: an exact box filter at 2:1, and a tent beyond that.
//!
//! Shaders rather than Direct3D 11's video processor: they run on any feature level 10 device,
//! including WARP, the software rasterizer behind VMs and CI runners, so the same path is
//! exercised everywhere, and the color math doesn't depend on each driver's video processor.

use std::sync::OnceLock;

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RENDER_TARGET_VIEW_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_RTV, D3D11_TEX2D_SRV, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_IMMUTABLE,
    D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
};
use windows::core::{Interface, PCSTR, s};

use super::frame::create_nv12_texture;

const SHADER: &str = r"
Texture2D<float4> source : register(t0);
SamplerState linear_clamp : register(s0);

cbuffer Constants : register(b0) {
    // The source texture coordinates the output spans.
    float2 extent;
    // How far each of the four taps lies from an output pixel's center.
    float2 tap;
};

struct Vertex {
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

// One triangle that covers the whole target.
Vertex vertex_main(uint id : SV_VertexID) {
    float2 corner = float2((id << 1) & 2, id & 2);
    Vertex vertex;
    vertex.position = float4(corner * float2(2, -2) + float2(-1, 1), 0, 1);
    vertex.uv = corner * extent;
    return vertex;
}

// The average color around `uv`, in 0..255 like OpenH264's.
float3 footprint(float2 uv) {
    float3 sum = source.Sample(linear_clamp, uv + float2(-tap.x, -tap.y)).rgb
        + source.Sample(linear_clamp, uv + float2(tap.x, -tap.y)).rgb
        + source.Sample(linear_clamp, uv + float2(-tap.x, tap.y)).rgb
        + source.Sample(linear_clamp, uv + float2(tap.x, tap.y)).rgb;
    return sum * (255.0 / 4.0);
}

// BT.601 limited range, truncated, as OpenH264 converts RGB.
float luma_main(Vertex vertex) : SV_Target {
    float3 rgb = footprint(vertex.uv);
    return floor(dot(rgb, float3(0.2578125, 0.50390625, 0.09765625)) + 16.0) / 255.0;
}

float2 chroma_main(Vertex vertex) : SV_Target {
    float3 rgb = footprint(vertex.uv);
    float u = dot(rgb, float3(-0.1484375, -0.2890625, 0.4375)) + 128.0;
    float v = dot(rgb, float3(0.4375, -0.3671875, -0.0703125)) + 128.0;
    return floor(float2(u, v)) / 255.0;
}
";

/// The compiled shaders, shared by every device.
struct Bytecode {
    vertex: Vec<u8>,
    luma: Vec<u8>,
    chroma: Vec<u8>,
}

fn bytecode() -> windows::core::Result<&'static Bytecode> {
    static COMPILED: OnceLock<Result<Bytecode, windows::core::HRESULT>> = OnceLock::new();
    COMPILED
        .get_or_init(|| {
            Ok(Bytecode {
                vertex: compile(s!("vertex_main"), s!("vs_4_0"))?,
                luma: compile(s!("luma_main"), s!("ps_4_0"))?,
                chroma: compile(s!("chroma_main"), s!("ps_4_0"))?,
            })
        })
        .as_ref()
        .map_err(|code| (*code).into())
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>, windows::core::HRESULT> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: The source pointer and length describe `SHADER`, the names are NUL-terminated
    // literals, and the out pointers are valid.
    let compiled = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("dari-convert"),
            None,
            None::<&ID3DInclude>,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &raw mut code,
            Some(&raw mut errors),
        )
    };
    if let Err(error) = compiled {
        if let Some(errors) = errors {
            tracing::warn!(message = %String::from_utf8_lossy(blob(&errors)), "shader compilation failed");
        }
        return Err(error.code());
    }
    code.map(|code| blob(&code).to_vec()).ok_or(E_POINTER)
}

fn blob(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: A blob owns `GetBufferSize` bytes at `GetBufferPointer` for as long as it lives.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

/// The shader constants of one pass; 16-byte aligned as constant buffers must be.
#[repr(C)]
struct PassConstants {
    extent: [f32; 2],
    tap: [f32; 2],
}

/// Scales the top-left `source` pixels of `input`-sized BGRA textures to `output` and converts
/// them to NV12.
pub(super) struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    lock: ID3D11Multithread,
    vertex: ID3D11VertexShader,
    luma: ID3D11PixelShader,
    chroma: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    luma_pass: ID3D11Buffer,
    chroma_pass: ID3D11Buffer,
    input: (u32, u32),
    source: (u32, u32),
    output: (u32, u32),
}

impl Converter {
    pub(super) fn new(
        device: &ID3D11Device,
        input: (u32, u32),
        source: (u32, u32),
        output: (u32, u32),
    ) -> windows::core::Result<Self> {
        let code = bytecode()?;
        let mut vertex = None;
        let mut luma = None;
        let mut chroma = None;
        let mut sampler = None;
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        // SAFETY: The bytecode came from the compiler for these shader stages, the descriptions
        // are valid, and the out pointers are valid.
        unsafe {
            device.CreateVertexShader(&code.vertex, None, Some(&raw mut vertex))?;
            device.CreatePixelShader(&code.luma, None, Some(&raw mut luma))?;
            device.CreatePixelShader(&code.chroma, None, Some(&raw mut chroma))?;
            device.CreateSamplerState(&raw const sampler_desc, Some(&raw mut sampler))?;
        }
        let missing = || windows::core::Error::from(E_POINTER);
        // Each output pixel covers `source / plane` source pixels; a chroma sample covers a 2×2
        // block of output pixels.
        let pass = |plane: (u32, u32)| {
            // How far apart the taps are, in texture coordinates, for each output pixel to
            // cover `source / plane` source pixels. At 1:1 or larger, all four taps coincide.
            let tap = |source: u32, plane: u32, texture: u32| {
                ((float(source) / float(plane) - 1.0).max(0.0) / 4.0) / float(texture)
            };
            PassConstants {
                extent: [
                    float(source.0) / float(input.0),
                    float(source.1) / float(input.1),
                ],
                tap: [
                    tap(source.0, plane.0, input.0),
                    tap(source.1, plane.1, input.1),
                ],
            }
        };
        Ok(Self {
            device: device.clone(),
            // SAFETY: Getting a device's immediate context has no preconditions.
            context: unsafe { device.GetImmediateContext() }?,
            lock: device.cast()?,
            vertex: vertex.ok_or_else(missing)?,
            luma: luma.ok_or_else(missing)?,
            chroma: chroma.ok_or_else(missing)?,
            sampler: sampler.ok_or_else(missing)?,
            luma_pass: constant_buffer(device, &pass(output))?,
            chroma_pass: constant_buffer(device, &pass((output.0 / 2, output.1 / 2)))?,
            input,
            source,
            output,
        })
    }

    pub(super) fn fits(&self, input: (u32, u32), source: (u32, u32), output: (u32, u32)) -> bool {
        (self.input, self.source, self.output) == (input, source, output)
    }

    /// Converts `input` into a new texture, so a frame still being encoded is never written to.
    pub(super) fn convert(
        &self,
        input: &ID3D11Texture2D,
    ) -> windows::core::Result<ID3D11Texture2D> {
        let output = create_nv12_texture(&self.device, self.output.0, self.output.1)?;
        let source = self.source_view(input)?;
        let luma = self.plane_view(&output, DXGI_FORMAT_R8_UNORM)?;
        let chroma = self.plane_view(&output, DXGI_FORMAT_R8G8_UNORM)?;
        // Windows.Graphics.Capture and the encoder use the same immediate context from their own
        // threads; hold the device's lock so their calls can't land between these.
        // SAFETY: Balanced by the `Leave` below, with nothing in between that can unwind.
        unsafe { self.lock.Enter() };
        // SAFETY: Every object belongs to this device, and the slices outlive the calls. The
        // views are unbound again before the lock is released.
        unsafe {
            let context = &self.context;
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShaderResources(0, Some(&[Some(source)]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            for (target, shader, constants, (width, height)) in [
                (luma, &self.luma, &self.luma_pass, self.output),
                (
                    chroma,
                    &self.chroma,
                    &self.chroma_pass,
                    (self.output.0 / 2, self.output.1 / 2),
                ),
            ] {
                context.VSSetConstantBuffers(0, Some(&[Some(constants.clone())]));
                context.PSSetConstantBuffers(0, Some(&[Some(constants.clone())]));
                context.PSSetShader(shader, None);
                context.OMSetRenderTargets(Some(&[Some(target)]), None);
                context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: float(width),
                    Height: float(height),
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]));
                context.Draw(3, 0);
            }
            context.OMSetRenderTargets(None, None);
            context.PSSetShaderResources(0, Some(&[None]));
            self.lock.Leave();
        }
        Ok(output)
    }

    fn source_view(
        &self,
        input: &ID3D11Texture2D,
    ) -> windows::core::Result<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut view = None;
        // SAFETY: `input` is a BGRA texture on this device, and the pointers are valid.
        unsafe {
            self.device
                .CreateShaderResourceView(input, Some(&raw const desc), Some(&raw mut view))
        }?;
        view.ok_or_else(|| E_POINTER.into())
    }

    /// A render target on one plane of an NV12 texture: R8 is luma, R8G8 the chroma pairs.
    fn plane_view(
        &self,
        texture: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<ID3D11RenderTargetView> {
        let desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
            },
        };
        let mut view = None;
        // SAFETY: `texture` is an NV12 render target on this device, and the pointers are valid.
        unsafe {
            self.device
                .CreateRenderTargetView(texture, Some(&raw const desc), Some(&raw mut view))
        }?;
        view.ok_or_else(|| E_POINTER.into())
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "texture sizes are far below 2^24, which f32 represents exactly"
)]
fn float(value: u32) -> f32 {
    value as f32
}

fn constant_buffer(
    device: &ID3D11Device,
    constants: &PassConstants,
) -> windows::core::Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: u32::try_from(size_of::<PassConstants>()).unwrap_or(u32::MAX),
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0.cast_unsigned(),
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: std::ptr::from_ref(constants).cast(),
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut buffer = None;
    // SAFETY: `initial` points at `ByteWidth` bytes that outlive the call.
    unsafe {
        device.CreateBuffer(
            &raw const desc,
            Some(&raw const initial),
            Some(&raw mut buffer),
        )
    }?;
    buffer.ok_or_else(|| E_POINTER.into())
}

#[cfg(test)]
mod tests {
    use openh264::formats::YUVSource;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

    use super::super::NativeFrame;
    use super::super::device::create_device;
    use super::*;
    use crate::codec::rgba_to_i420;
    use crate::frame::RgbaFrame;
    use crate::synthetic::SyntheticCapturer;

    /// A BGRA texture of `size` with `frame` in its top-left corner and white around it, like a
    /// capture buffer larger than the display.
    fn capture_texture(
        device: &ID3D11Device,
        frame: &RgbaFrame,
        size: (u32, u32),
    ) -> ID3D11Texture2D {
        let mut bgra = vec![255u8; size.0 as usize * size.1 as usize * 4];
        for (row, source) in frame
            .pixels()
            .chunks(frame.width() as usize * 4)
            .enumerate()
        {
            let target = &mut bgra[row * size.0 as usize * 4..][..source.len()];
            for (target, source) in target
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(source.as_chunks::<4>().0)
            {
                *target = [source[2], source[1], source[0], source[3]];
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0.cast_unsigned(),
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr().cast(),
            SysMemPitch: size.0 * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: `initial` holds `size` BGRA pixels, rows `size.0 * 4` bytes apart.
        unsafe {
            device.CreateTexture2D(
                &raw const desc,
                Some(&raw const initial),
                Some(&raw mut texture),
            )
        }
        .unwrap();
        texture.unwrap()
    }

    /// Mean and largest absolute difference between two planes.
    fn difference(gpu: &[u8], cpu: &[u8]) -> (f64, u8) {
        assert_eq!(gpu.len(), cpu.len());
        let largest = gpu
            .iter()
            .zip(cpu)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        let total: u64 = gpu
            .iter()
            .zip(cpu)
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        #[expect(clippy::cast_precision_loss)]
        let mean = total as f64 / gpu.len() as f64;
        (mean, largest)
    }

    #[test]
    fn colors_match_openh264() {
        let device = create_device().unwrap();
        let source = SyntheticCapturer::new(64, 48).render();
        let texture = capture_texture(&device, &source, (64, 48));
        let converter = Converter::new(&device, (64, 48), (64, 48), (64, 48)).unwrap();
        let frame = NativeFrame::new(converter.convert(&texture).unwrap()).unwrap();
        let gpu = frame.to_i420().unwrap();
        let cpu = rgba_to_i420(&source);
        let (luma, chroma) = gpu.split_at(64 * 48);
        let (u, v) = chroma.split_at(32 * 24);
        let (mean, largest) = difference(luma, cpu.y());
        assert!(
            mean < 1.5 && largest <= 4,
            "luma differs by {mean} on average, {largest} at most"
        );
        for (name, gpu, cpu) in [("U", u, cpu.u()), ("V", v, cpu.v())] {
            let (mean, largest) = difference(gpu, cpu);
            // Chroma is subsampled; the GPU may filter or site it differently at the edges of the
            // moving square.
            assert!(
                mean < 3.0,
                "{name} differs by {mean} on average, {largest} at most"
            );
        }
    }

    #[test]
    fn only_the_display_area_is_scaled() {
        let device = create_device().unwrap();
        let color = [40, 160, 220, 255];
        let source = RgbaFrame::new(64, 48, color.repeat(64 * 48)).unwrap();
        // The pool's buffer is larger than the display; the white margin must not show.
        let texture = capture_texture(&device, &source, (96, 64));
        let converter = Converter::new(&device, (96, 64), (64, 48), (32, 24)).unwrap();
        let frame = NativeFrame::new(converter.convert(&texture).unwrap()).unwrap();
        assert_eq!((frame.width(), frame.height()), (32, 24));
        let expected = rgba_to_i420(&RgbaFrame::new(32, 24, color.repeat(32 * 24)).unwrap());
        let gpu = frame.to_i420().unwrap();
        let (luma, chroma) = gpu.split_at(32 * 24);
        let (u, v) = chroma.split_at(16 * 12);
        for (name, gpu, cpu) in [
            ("Y", luma, expected.y()),
            ("U", u, expected.u()),
            ("V", v, expected.v()),
        ] {
            let (_, largest) = difference(gpu, cpu);
            assert!(largest <= 3, "{name} differs by up to {largest}");
        }
    }

    #[test]
    fn halving_averages_instead_of_skipping_pixels() {
        let device = create_device().unwrap();
        // Alternating black and white columns: sampling every other pixel would see one color.
        let pixels: Vec<u8> = (0..64 * 48)
            .flat_map(|index| {
                if index % 2 == 0 {
                    [0, 0, 0, 255]
                } else {
                    [255; 4]
                }
            })
            .collect();
        let source = RgbaFrame::new(64, 48, pixels).unwrap();
        let texture = capture_texture(&device, &source, (64, 48));
        let converter = Converter::new(&device, (64, 48), (64, 48), (32, 24)).unwrap();
        let frame = NativeFrame::new(converter.convert(&texture).unwrap()).unwrap();
        let gpu = frame.to_i420().unwrap();
        let gray = rgba_to_i420(&RgbaFrame::new(2, 2, [127, 127, 127, 255].repeat(4)).unwrap());
        let (luma, chroma) = gpu.split_at(32 * 24);
        assert!(
            luma.iter().all(|y| y.abs_diff(gray.y()[0]) <= 1),
            "luma {luma:?}"
        );
        assert!(
            chroma.iter().all(|c| c.abs_diff(128) <= 1),
            "chroma {chroma:?}"
        );
    }
}
