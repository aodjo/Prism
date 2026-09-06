//! Turning captured BGRA frames into the NV12 an encoder wants, on the GPU.
//!
//! Windows.Graphics.Capture delivers BGRA and every hardware H.264 encoder wants NV12, so
//! something has to convert. Doing it on the CPU would mean reading the frame back out of
//! GPU memory — four hundred megabytes a second at 1440p120 — which would undo the entire
//! reason the capture path was built the way it was. So it is two draw calls instead: one
//! filling the luma plane at full resolution, one filling the chroma plane at half.
//!
//! # Why there is no linearisation
//!
//! BT.709's `Y'CbCr` is defined over **gamma-encoded** R'G'B', not linear light. The
//! captured BGRA is already gamma-encoded, so the matrix applies to it directly. Converting
//! to linear light first is a well-travelled mistake that produces a picture which looks
//! plausible and is wrong everywhere except black and white.
//!
//! # Why chroma is sampled rather than averaged
//!
//! The chroma pass runs at half resolution with a bilinear sampler, so each output texel is
//! the average of the four input texels beneath it — which is the box filter chroma
//! subsampling wants. It is correct to average after the matrix rather than before because
//! the matrix is linear in R'G'B', so the two orders give the same answer.
//!
//! The values written are **video range**: luma spans 16 to 235 and chroma 16 to 240. That
//! is the same convention the Metal renderer decodes, which is what makes the two shaders
//! checkable against each other rather than only against themselves.

use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_COMPARISON_NEVER,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_TEX2D_RTV, D3D11_TEX2D_SRV,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_SAMPLE_DESC,
};
use windows::core::PCSTR;

use crate::encode::EncodeError;

/// The conversion shader.
///
/// Compiled at startup from source held here rather than shipped as bytecode, so the
/// renderer stays independent of the build machine's shader toolchain — the same choice the
/// Metal path makes for the same reason.
const SHADER: &str = r#"
struct VOut {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

// A full-screen triangle rather than a quad: no vertex buffer to manage and no seam down
// the diagonal.
VOut vs_main(uint vid : SV_VertexID) {
    const float2 corners[3] = { float2(-1, -3), float2(-1, 1), float2(3, 1) };
    float2 p = corners[vid];

    VOut o;
    o.pos = float4(p, 0.0, 1.0);
    o.uv = float2((p.x + 1.0) * 0.5, 1.0 - (p.y + 1.0) * 0.5);
    return o;
}

Texture2D<float4> source : register(t0);
SamplerState bilinear : register(s0);

// BT.709, gamma-encoded RGB in, video-range Y'CbCr out. The inverse of what the Metal
// renderer applies when it draws a decoded picture.
float3 to_ycbcr(float3 rgb) {
    float y  = dot(rgb, float3(0.2126, 0.7152, 0.0722));
    float cb = (rgb.b - y) / 1.8556;
    float cr = (rgb.r - y) / 1.5748;

    return float3(
        16.0 / 255.0 + y * (219.0 / 255.0),
        128.0 / 255.0 + cb * (224.0 / 255.0),
        128.0 / 255.0 + cr * (224.0 / 255.0)
    );
}

float ps_luma(VOut i) : SV_TARGET {
    return to_ycbcr(source.Sample(bilinear, i.uv).rgb).x;
}

float2 ps_chroma(VOut i) : SV_TARGET {
    return to_ycbcr(source.Sample(bilinear, i.uv).rgb).yz;
}
"#;

/// An NV12 texture on the GPU, with a view onto each of its two planes.
///
/// NV12 is planar, and Direct3D selects the plane by the format of the view rather than by
/// an index: `R8_UNORM` reaches the luma plane and `R8G8_UNORM` the interleaved chroma one.
pub struct Nv12Texture {
    texture: ID3D11Texture2D,
    luma: ID3D11RenderTargetView,
    chroma: ID3D11RenderTargetView,
    width: u32,
    height: u32,
}

impl Nv12Texture {
    /// Allocates an NV12 texture the encoder can read and the converter can draw into.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::InputBuffer`] if the dimensions are not even, or if Direct3D
    /// will not allocate the texture or its plane views.
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self, EncodeError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(EncodeError::InputBuffer {
                reason: "NV12 subsamples chroma by two, so both dimensions must be even",
            });
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut texture: Option<ID3D11Texture2D> = None;

        // SAFETY: the description is fully initialised and the output parameter is a live
        // local that is only read when the call reports success.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(|err| {
            EncodeError::SessionCreate {
                reason: "could not allocate an NV12 texture",
                status: err.code().0,
            }
        })?;

        let texture = texture.ok_or(EncodeError::InputBuffer {
            reason: "Direct3D reported success but produced no NV12 texture",
        })?;

        let luma = plane_view(device, &texture, DXGI_FORMAT_R8_UNORM)?;
        let chroma = plane_view(device, &texture, DXGI_FORMAT_R8G8_UNORM)?;

        Ok(Self {
            texture,
            luma,
            chroma,
            width,
            height,
        })
    }

    /// Returns the texture itself, for handing to an encoder.
    #[must_use]
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// Returns the texture's width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Returns the texture's height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }
}

impl core::fmt::Debug for Nv12Texture {
    /// Describes the texture without reaching into COM objects.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Nv12Texture")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Converts captured BGRA frames into NV12 without leaving the GPU.
pub struct Bgra2Nv12 {
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    luma: ID3D11PixelShader,
    chroma: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
}

impl Bgra2Nv12 {
    /// Compiles the conversion shaders on the given device.
    ///
    /// The device must be the one the captured frames belong to. Two devices would mean a
    /// copy across them for every frame, which is the copy this exists to avoid.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::SessionCreate`] if a shader will not compile or Direct3D will
    /// not create one of the objects the pass needs.
    pub fn new(device: &ID3D11Device) -> Result<Self, EncodeError> {
        // SAFETY: the device is alive and the call only writes through the pointer it is
        // given, which is a live local.
        let context =
            unsafe { device.GetImmediateContext() }.map_err(|err| EncodeError::SessionCreate {
                reason: "the device has no immediate context",
                status: err.code().0,
            })?;

        let vertex_code = compile("vs_main", "vs_5_0")?;
        let luma_code = compile("ps_luma", "ps_5_0")?;
        let chroma_code = compile("ps_chroma", "ps_5_0")?;

        // SAFETY: each blob holds compiled bytecode of the stage it is being created for,
        // and the output parameters are live locals.
        let (vertex, luma, chroma) = unsafe {
            let mut vertex = None;
            device
                .CreateVertexShader(&vertex_code, None, Some(&mut vertex))
                .map_err(|err| shader_error("vertex shader", err))?;

            let mut luma = None;
            device
                .CreatePixelShader(&luma_code, None, Some(&mut luma))
                .map_err(|err| shader_error("luma shader", err))?;

            let mut chroma = None;
            device
                .CreatePixelShader(&chroma_code, None, Some(&mut chroma))
                .map_err(|err| shader_error("chroma shader", err))?;

            (
                vertex.ok_or(missing("vertex shader"))?,
                luma.ok_or(missing("luma shader"))?,
                chroma.ok_or(missing("chroma shader"))?,
            )
        };

        // Bilinear and clamped. Bilinear is what makes the half-resolution chroma pass a
        // box filter over the four texels beneath each output texel rather than a point
        // sample of one of them; clamping keeps the edge texels from wrapping.
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };

        // SAFETY: the description is fully initialised and the output is a live local.
        let sampler = unsafe {
            let mut sampler = None;
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(|err| shader_error("sampler", err))?;
            sampler.ok_or(missing("sampler"))?
        };

        Ok(Self {
            context,
            vertex,
            luma,
            chroma,
            sampler,
        })
    }

    /// Converts one BGRA frame into an NV12 texture.
    ///
    /// Two passes because the planes are different sizes: luma at full resolution, chroma at
    /// half. Nothing is read back and nothing is allocated.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::InputBuffer`] if the source cannot be bound as a texture.
    pub fn convert(
        &self,
        source: &ID3D11Texture2D,
        target: &Nv12Texture,
    ) -> Result<(), EncodeError> {
        let view = self.source_view(source)?;

        // SAFETY: every object bound below outlives the calls, the shaders match the stages
        // they are set on, and the draw emits exactly the three vertices the vertex shader
        // generates without reading any buffer.
        unsafe {
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShaderResources(0, Some(&[Some(view)]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));

            self.draw_plane(&target.luma, &self.luma, target.width, target.height);
            self.draw_plane(
                &target.chroma,
                &self.chroma,
                target.width / 2,
                target.height / 2,
            );

            // Unbound so the source texture is not still referenced when the caller returns
            // its frame to the capture pool.
            self.context.PSSetShaderResources(0, Some(&[None]));
        }

        Ok(())
    }

    /// Draws the full-screen triangle into one plane.
    ///
    /// # Safety
    ///
    /// The view and shader must belong to the device this converter was created on.
    unsafe fn draw_plane(
        &self,
        target: &ID3D11RenderTargetView,
        shader: &ID3D11PixelShader,
        width: u32,
        height: u32,
    ) {
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        // SAFETY: the caller guarantees the view and shader belong to this device, and the
        // viewport covers exactly the plane being drawn.
        unsafe {
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            self.context.PSSetShader(shader, None);
            self.context.Draw(3, 0);
        }
    }

    /// Binds a captured BGRA texture so the shaders can sample it.
    fn source_view(
        &self,
        source: &ID3D11Texture2D,
    ) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView, EncodeError> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };

        // SAFETY: the device outlives the call, the description is fully initialised, and
        // the output is a live local read only on success.
        let view = unsafe {
            let device = self.context.GetDevice().map_err(|_| missing("device"))?;

            let mut view = None;
            device
                .CreateShaderResourceView(source, Some(&desc), Some(&mut view))
                .map_err(|_| EncodeError::InputBuffer {
                    reason: "the captured frame could not be bound as a shader resource",
                })?;
            view.ok_or(EncodeError::InputBuffer {
                reason: "Direct3D reported success but produced no shader resource view",
            })?
        };

        Ok(view)
    }
}

impl core::fmt::Debug for Bgra2Nv12 {
    /// Describes the converter without reaching into COM objects.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bgra2Nv12").finish_non_exhaustive()
    }
}

/// Builds a render target view onto one plane of an NV12 texture.
///
/// The plane is chosen by the view's format, which is how Direct3D addresses planar
/// surfaces: `R8_UNORM` is the luma plane and `R8G8_UNORM` the chroma one.
fn plane_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<ID3D11RenderTargetView, EncodeError> {
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
        },
    };

    // SAFETY: the description is fully initialised for a two-dimensional view and the
    // output is a live local read only on success.
    let view = unsafe {
        let mut view = None;
        device
            .CreateRenderTargetView(texture, Some(&desc), Some(&mut view))
            .map_err(|err| EncodeError::SessionCreate {
                reason: "could not create a view onto an NV12 plane",
                status: err.code().0,
            })?;
        view.ok_or(missing("plane view"))?
    };

    Ok(view)
}

/// Compiles one entry point of the shader source.
fn compile(entry: &str, target: &str) -> Result<Vec<u8>, EncodeError> {
    let entry = std::ffi::CString::new(entry).expect("entry point names hold no interior nul");
    let target = std::ffi::CString::new(target).expect("shader targets hold no interior nul");

    let mut code = None;
    let mut errors = None;

    // SAFETY: the source is a live slice for the duration of the call, both name pointers
    // are nul-terminated and live, and the outputs are live locals.
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            None,
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };

    if result.is_err() {
        return Err(EncodeError::SessionCreate {
            reason: "a conversion shader would not compile",
            status: result.err().map_or(0, |err| err.code().0),
        });
    }

    let code = code.ok_or(EncodeError::SessionCreate {
        reason: "the shader compiler reported success but produced no bytecode",
        status: 0,
    })?;

    // SAFETY: the blob is alive and reports its own buffer and length.
    let bytes = unsafe {
        core::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };

    Ok(bytes.to_vec())
}

/// Reports a Direct3D object that could not be created.
fn shader_error(what: &'static str, error: windows::core::Error) -> EncodeError {
    let _ = what;

    EncodeError::SessionCreate {
        reason: "Direct3D refused a conversion shader object",
        status: error.code().0,
    }
}

/// Reports a Direct3D call that succeeded without producing what it promised.
fn missing(_what: &'static str) -> EncodeError {
    EncodeError::SessionCreate {
        reason: "Direct3D reported success but produced no object",
        status: 0,
    }
}
