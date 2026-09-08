//! Direct3D 11 renderer for decoded pictures.
//!
//! Media Foundation decodes into an NV12 `ID3D11Texture2D` that already lives in GPU memory.
//! Rather than reading it back and converting on the CPU — four megabytes a frame at 1440p,
//! every frame — the two planes are bound as shader resource views and converted in a pixel
//! shader. Direct3D selects the plane by the view's format rather than by an index:
//! `R8_UNORM` reaches the luma plane and `R8G8_UNORM` the interleaved chroma one, the same
//! convention the encoder's NV12 conversion writes with.
//!
//! # Why the swap chain is built the way it is
//!
//! Flip model with tearing allowed and a maximum frame latency of one. Every part of that is
//! latency: the older blt model copies the back buffer through the desktop compositor, which
//! costs a frame; a deeper latency queue lets Direct3D accept frames faster than the display
//! shows them, which costs however many it has queued; and waiting for the vertical blank to
//! hand over a frame that is already finished costs up to another. This is the same trade the
//! Metal renderer makes by turning display sync off and holding two drawables.
//!
//! The waitable object is then used the way the Metal path uses `nextDrawable`: polled with no
//! timeout, and a frame is dropped rather than queued when the display is behind. A frame
//! shown late is worse than one not shown at all.

use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BLEND_DESC,
    D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BUFFER_DESC,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAP_WRITE_DISCARD,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_ARRAY_SRV, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT, ID3D11BlendState, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2,
    IDXGIFactory5, IDXGISwapChain1, IDXGISwapChain2,
};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::{Interface, PCSTR};

use crate::render::RenderError;

/// The shaders that draw a picture and blend the overlays over it.
///
/// Compiled at startup from source held here rather than shipped as bytecode, so the renderer
/// stays independent of the build machine's shader toolchain — the same choice the Metal path
/// makes for the same reason. The colour conversion is BT.709 video range, which is what the
/// encoder writes and what the Metal renderer decodes, so the two can be checked against each
/// other rather than only against themselves.
const SHADER: &str = r#"
struct VOut {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

// A full-screen triangle rather than a quad, so there is no vertex buffer to manage and no
// seam down the diagonal. It covers the whole target, which is why nothing clears first.
VOut vs_picture(uint vid : SV_VertexID) {
    const float2 corners[3] = { float2(-1, -3), float2(-1, 1), float2(3, 1) };
    float2 p = corners[vid];

    VOut o;
    o.pos = float4(p, 0.0, 1.0);
    o.uv = float2((p.x + 1.0) * 0.5, 1.0 - (p.y + 1.0) * 0.5);
    return o;
}

// Every resource below has a register of its own, including across the two pixel shaders that
// never run together. Two different resources declared on one register is a thing the
// compiler is entitled to resolve either way, and a renderer that samples the wrong texture
// draws a black window with nothing reporting an error.
Texture2D<float>  luma     : register(t0);
Texture2D<float2> chroma   : register(t1);
SamplerState      smooth_  : register(s0);

float4 ps_picture(VOut i) : SV_TARGET {
    float y = luma.Sample(smooth_, i.uv);
    float2 cbcr = chroma.Sample(smooth_, i.uv);

    y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    float cb = (cbcr.x - 128.0 / 255.0) * (255.0 / 224.0);
    float cr = (cbcr.y - 128.0 / 255.0) * (255.0 / 224.0);

    float3 rgb = float3(
        y + 1.5748 * cr,
        y - 0.1873 * cb - 0.4681 * cr,
        y + 1.8556 * cb
    );

    return float4(saturate(rgb), 1.0);
}

// `rect` is (left, top, width, height) in normalised device coordinates, where y grows
// upwards, which is what `render::place` produces. No vertical flip: the bitmaps are laid out
// with their first row at the top, so v zero is already the top of the image.
cbuffer Placement : register(b0) {
    float4 rect;
};

VOut vs_overlay(uint vid : SV_VertexID) {
    const float2 corners[6] = {
        float2(0, 0), float2(1, 0), float2(0, 1),
        float2(1, 0), float2(1, 1), float2(0, 1)
    };
    float2 c = corners[vid];

    VOut o;
    o.pos = float4(rect.x + c.x * rect.z, rect.y - c.y * rect.w, 0.0, 1.0);
    o.uv = c;
    return o;
}

Texture2D<float4> glyphs : register(t2);
SamplerState      sharp  : register(s1);

float4 ps_overlay(VOut i) : SV_TARGET {
    return glyphs.Sample(sharp, i.uv);
}
"#;

/// Where the picture's luma plane is bound.
const LUMA_SLOT: u32 = 0;

/// Where an overlay's bitmap is bound.
///
/// Past the picture's two planes, so an overlay draw does not have to unbind them and the
/// picture draw does not have to unbind the overlay.
const GLYPH_SLOT: u32 = 2;

/// Where the sampler the picture is scaled with is bound.
const SMOOTH_SLOT: u32 = 0;

/// Where the sampler the overlays are drawn with is bound.
const SHARP_SLOT: u32 = 1;

/// How many back buffers the swap chain holds.
///
/// Two, the minimum the flip model accepts. A third would let Direct3D keep another finished
/// frame waiting its turn, which is exactly the delay this path exists to avoid.
const BUFFER_COUNT: u32 = 2;

/// One textured rectangle blended over the picture.
///
/// The statistics overlay and the cursor are the same operation — an RGBA bitmap with
/// premultiplied alpha, drawn somewhere on the target — so the renderer takes a list of them
/// rather than knowing what either one is.
#[derive(Debug, Clone)]
pub struct Quad<'a> {
    /// The bitmap to sample, with premultiplied alpha.
    pub texture: &'a ID3D11ShaderResourceView,
    /// Left, top, width and height in normalised device coordinates, where y grows upwards.
    ///
    /// [`crate::render::place`] builds this from a position and a pixel size.
    pub rect: [f32; 4],
}

/// Views onto one picture's two planes, remembered for as long as the texture is reused.
///
/// A decoder hands back the same handful of textures over and over, so the views onto them
/// are built once each rather than per frame. The texture is identified by its interface
/// pointer, which is only ever compared, never followed.
#[derive(Debug)]
struct Bound {
    texture: usize,
    index: u32,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
}

/// A texture pictures are copied into when the decoder's own cannot be sampled.
///
/// Direct3D only lets a texture be read by a shader if it was created saying so, and a
/// decoder's output textures are created by the decoder. Most drivers ask for both, some do
/// not, and the ones that do not would otherwise be a black window. The copy stays on the GPU.
#[derive(Debug)]
struct Scratch {
    texture: ID3D11Texture2D,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

/// Draws decoded pictures into a window.
pub struct D3d11Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    /// The view onto the back buffer, absent only while the swap chain is being resized —
    /// which is the one moment nothing may be holding a reference to the buffer it names.
    target: Option<ID3D11RenderTargetView>,
    picture_vs: ID3D11VertexShader,
    picture_ps: ID3D11PixelShader,
    overlay_vs: ID3D11VertexShader,
    overlay_ps: ID3D11PixelShader,
    bilinear: ID3D11SamplerState,
    nearest: ID3D11SamplerState,
    blend: ID3D11BlendState,
    placement: ID3D11Buffer,
    /// Signalled when the display will accept another frame, or null if the swap chain has no
    /// waitable object. A null handle means every frame is presented, which is what the older
    /// behaviour was and is still correct, only less well paced.
    waitable: HANDLE,
    tearing: bool,
    width: u32,
    height: u32,
    bound: Vec<Bound>,
    scratch: Option<Scratch>,
    /// Set once the decoder's textures have been found unsamplable, so it is discovered once
    /// rather than attempted per frame.
    copying: bool,
}

impl D3d11Renderer {
    /// Builds a renderer drawing into a window.
    ///
    /// `device` must be the device the decoder produces its pictures on. Two devices would
    /// mean copying every picture between them, which is the copy this whole path exists to
    /// avoid.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Shader`] if a shader will not compile and [`RenderError::Setup`]
    /// if Direct3D or DXGI refuses one of the objects the renderer needs.
    ///
    /// # Safety
    ///
    /// `hwnd` must be a live window handle owned by the calling thread.
    pub unsafe fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> Result<Self, RenderError> {
        let factory = factory_for(device)?;
        let tearing = supports_tearing(&factory);

        let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0;
        if tearing {
            flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0;
        }

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFER_COUNT,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags as u32,
        };

        // SAFETY: the caller guarantees the window handle is live, the description is fully
        // initialised, and the device outlives the swap chain built on it.
        let swapchain = unsafe {
            factory
                .CreateSwapChainForHwnd(device, hwnd, &desc, None, None)
                .map_err(|_| RenderError::Setup {
                    reason: "could not create a swap chain for the window",
                })?
        };

        // Alt+Enter belongs to the window's own toolkit, not to DXGI. Left alone it takes the
        // stream fullscreen behind the toolkit's back and neither side agrees on the size.
        // SAFETY: the handle is the one the swap chain was just built for.
        let _ = unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) };

        let waitable = configure_latency(&swapchain);
        let target = Some(back_buffer_view(device, &swapchain)?);

        let picture_vs = vertex_shader(device, "vs_picture")?;
        let overlay_vs = vertex_shader(device, "vs_overlay")?;
        let picture_ps = pixel_shader(device, "ps_picture")?;
        let overlay_ps = pixel_shader(device, "ps_overlay")?;

        // Bilinear for the picture so it scales smoothly to a window that is rarely the size
        // of the host's screen; point for the overlays so text and the cursor stay crisp
        // rather than being blurred by the same filter.
        let bilinear = sampler(device, D3D11_FILTER_MIN_MAG_MIP_LINEAR)?;
        let nearest = sampler(device, D3D11_FILTER_MIN_MAG_MIP_POINT)?;

        let blend = premultiplied_blend(device)?;
        let placement = placement_buffer(device)?;

        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            swapchain,
            target,
            picture_vs,
            picture_ps,
            overlay_vs,
            overlay_ps,
            bilinear,
            nearest,
            blend,
            placement,
            waitable,
            tearing,
            width: width.max(1),
            height: height.max(1),
            bound: Vec::new(),
            scratch: None,
            copying: false,
        })
    }

    /// Returns the device the renderer draws on.
    #[must_use]
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Returns whether the swap chain will present without waiting for the vertical blank.
    #[must_use]
    pub fn tearing(&self) -> bool {
        self.tearing
    }

    /// Resizes the swap chain to a new window size.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if DXGI will not resize the buffers or Direct3D will not
    /// build a view onto the new back buffer.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        let (width, height) = (width.max(1), height.max(1));
        if width == self.width && height == self.height {
            return Ok(());
        }

        let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
        if self.tearing {
            flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
        }

        // DXGI refuses to resize a buffer that anything still refers to, and the view is a
        // reference every bit as much as the binding on the context is. Both have to go: the
        // state is cleared and the view dropped before the resize, not merely unbound.
        // Everything cleared here is set again by the next draw.
        self.target = None;

        // SAFETY: the context is the one every draw was recorded on, and the swap chain is the
        // one those draws are presented through.
        unsafe {
            self.context.ClearState();
            self.swapchain
                .ResizeBuffers(
                    BUFFER_COUNT,
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    flags,
                )
                .map_err(|_| RenderError::Setup {
                    reason: "could not resize the swap chain",
                })?;
        }

        self.target = Some(back_buffer_view(&self.device, &self.swapchain)?);
        self.width = width;
        self.height = height;

        Ok(())
    }

    /// Uploads an RGBA bitmap as a texture the renderer can blend as a [`Quad`].
    ///
    /// The bitmap must hold `width * height * 4` bytes with premultiplied alpha, laid out one
    /// row after another from the top.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if the bitmap is the wrong length, or if Direct3D will
    /// not allocate the texture or a view onto it.
    pub fn upload_bitmap(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Result<ID3D11ShaderResourceView, RenderError> {
        if pixels.len() != (width as usize) * (height as usize) * 4 {
            return Err(RenderError::Setup {
                reason: "the bitmap is not the size its dimensions say",
            });
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };

        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };

        // SAFETY: the description is fully initialised, the initial data points at exactly the
        // bytes the description covers, and the output is a live local.
        let texture = unsafe {
            let mut texture = None;
            self.device
                .CreateTexture2D(&desc, Some(&initial), Some(&mut texture))
                .map_err(|_| RenderError::Setup {
                    reason: "could not allocate an overlay texture",
                })?;
            texture.ok_or(RenderError::Setup {
                reason: "Direct3D reported success but produced no overlay texture",
            })?
        };

        plane_view(&self.device, &texture, DXGI_FORMAT_R8G8B8A8_UNORM, 0)
    }

    /// Draws one picture into the back buffer and presents it.
    ///
    /// `index` is the slice of `texture` the picture occupies, because a transform that
    /// decodes into an array hands back the same texture for every picture.
    ///
    /// Returns `false` when the display is still busy with the frame before, in which case
    /// nothing is drawn: a frame shown late is worse than one not shown at all.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] if the picture's planes cannot be bound as textures.
    pub fn present(
        &mut self,
        texture: &ID3D11Texture2D,
        index: u32,
        quads: &[Quad<'_>],
    ) -> Result<bool, RenderError> {
        if !self.display_is_ready() {
            return Ok(false);
        }

        let target = self.target.clone().ok_or(RenderError::Setup {
            reason: "the swap chain has no back buffer to draw into",
        })?;
        let (luma, chroma) = self.planes(texture, index)?;

        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        // SAFETY: every object bound below belongs to this renderer's device and outlives the
        // calls; the picture draw emits exactly the three vertices its vertex shader generates
        // and each overlay the six of its own, neither reading any vertex buffer. Nothing
        // clears first because the full-screen triangle covers the whole target.
        unsafe {
            self.context.OMSetRenderTargets(Some(&[Some(target)]), None);
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

            self.context.OMSetBlendState(None, None, u32::MAX);
            self.context.VSSetShader(&self.picture_vs, None);
            self.context.PSSetShader(&self.picture_ps, None);
            self.context
                .PSSetSamplers(SMOOTH_SLOT, Some(&[Some(self.bilinear.clone())]));
            self.context
                .PSSetShaderResources(LUMA_SLOT, Some(&[Some(luma), Some(chroma)]));
            self.context.Draw(3, 0);

            if !quads.is_empty() {
                self.context.OMSetBlendState(&self.blend, None, u32::MAX);
                self.context.VSSetShader(&self.overlay_vs, None);
                self.context.PSSetShader(&self.overlay_ps, None);
                self.context
                    .PSSetSamplers(SHARP_SLOT, Some(&[Some(self.nearest.clone())]));
                self.context
                    .VSSetConstantBuffers(0, Some(&[Some(self.placement.clone())]));

                // Drawn in the order given, so a caller decides what sits on top of what by
                // where it puts it in the slice.
                for quad in quads {
                    self.write_placement(quad.rect);
                    self.context
                        .PSSetShaderResources(GLYPH_SLOT, Some(&[Some(quad.texture.clone())]));
                    self.context.Draw(6, 0);
                }

                self.context.PSSetShaderResources(GLYPH_SLOT, Some(&[None]));
            }

            // Unbound so the decoder's texture is not still referenced when it is handed back
            // to the pool the transform decodes into.
            self.context
                .PSSetShaderResources(LUMA_SLOT, Some(&[None, None]));

            let flags = if self.tearing {
                DXGI_PRESENT_ALLOW_TEARING
            } else {
                DXGI_PRESENT(0)
            };
            self.swapchain
                .Present(0, flags)
                .ok()
                .map_err(|err| RenderError::Bind {
                    status: err.code().0,
                })?;
        }

        Ok(true)
    }

    /// Returns whether the display will accept another frame right now.
    fn display_is_ready(&self) -> bool {
        if self.waitable.is_invalid() {
            return true;
        }

        // SAFETY: the handle came from the swap chain this renderer owns and stays valid for
        // as long as it does. A zero timeout makes this a poll rather than a wait.
        unsafe { WaitForSingleObjectEx(self.waitable, 0, false) == WAIT_OBJECT_0 }
    }

    /// Writes one overlay's rectangle into the constant buffer the vertex shader reads.
    ///
    /// # Safety
    ///
    /// Must be called between binding and drawing, on the context that owns the buffer.
    unsafe fn write_placement(&self, rect: [f32; 4]) {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();

        // SAFETY: the buffer is dynamic and write-only, which is what `WRITE_DISCARD`
        // requires, and it is exactly four floats long — the size written below.
        unsafe {
            if self
                .context
                .Map(
                    &self.placement,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .is_err()
            {
                return;
            }

            mapped.pData.cast::<[f32; 4]>().write(rect);
            self.context.Unmap(&self.placement, 0);
        }
    }

    /// Returns views onto a picture's two planes, building them the first time it is seen.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] if neither binding the picture where it lies nor copying
    /// it somewhere that can be bound works.
    fn planes(
        &mut self,
        texture: &ID3D11Texture2D,
        index: u32,
    ) -> Result<(ID3D11ShaderResourceView, ID3D11ShaderResourceView), RenderError> {
        if !self.copying {
            let key = texture.as_raw() as usize;

            if let Some(bound) = self
                .bound
                .iter()
                .find(|bound| bound.texture == key && bound.index == index)
            {
                return Ok((bound.luma.clone(), bound.chroma.clone()));
            }

            match self.bind(texture, index) {
                Ok((luma, chroma)) => {
                    self.bound.push(Bound {
                        texture: key,
                        index,
                        luma: luma.clone(),
                        chroma: chroma.clone(),
                    });
                    return Ok((luma, chroma));
                }
                // Discovered once rather than attempted per frame: a decoder that does not
                // create its textures for sampling never will.
                Err(_) => self.copying = true,
            }
        }

        self.copy(texture, index)
    }

    /// Builds views onto one slice of a decoder's own texture.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] if the texture was not created to be sampled.
    fn bind(
        &self,
        texture: &ID3D11Texture2D,
        index: u32,
    ) -> Result<(ID3D11ShaderResourceView, ID3D11ShaderResourceView), RenderError> {
        let luma = plane_view(&self.device, texture, DXGI_FORMAT_R8_UNORM, index)?;
        let chroma = plane_view(&self.device, texture, DXGI_FORMAT_R8G8_UNORM, index)?;

        Ok((luma, chroma))
    }

    /// Copies one slice of a decoder's texture into one that can be sampled.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] if the scratch texture or its views cannot be created.
    fn copy(
        &mut self,
        texture: &ID3D11Texture2D,
        index: u32,
    ) -> Result<(ID3D11ShaderResourceView, ID3D11ShaderResourceView), RenderError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: the texture is alive for the duration of the call.
        unsafe { texture.GetDesc(&mut desc) };

        let stale = self
            .scratch
            .as_ref()
            .is_none_or(|scratch| scratch.width != desc.Width || scratch.height != desc.Height);

        if stale {
            self.scratch = Some(self.new_scratch(desc.Width, desc.Height)?);
        }

        let scratch = self
            .scratch
            .as_ref()
            .ok_or(RenderError::Bind { status: 0 })?;

        // SAFETY: both textures are NV12 of the same size on the same device, and the whole
        // surface is copied, which is what a null source box means. This stays on the GPU.
        unsafe {
            self.context
                .CopySubresourceRegion(&scratch.texture, 0, 0, 0, 0, texture, index, None);
        }

        Ok((scratch.luma.clone(), scratch.chroma.clone()))
    }

    /// Allocates the texture pictures are copied into, with a view onto each plane.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] if Direct3D will not allocate it.
    fn new_scratch(&self, width: u32, height: u32) -> Result<Scratch, RenderError> {
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
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };

        // SAFETY: the description is fully initialised and the output is a live local.
        let texture = unsafe {
            let mut texture = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|err| RenderError::Bind {
                    status: err.code().0,
                })?;
            texture.ok_or(RenderError::Bind { status: 0 })?
        };

        let luma = plane_view(&self.device, &texture, DXGI_FORMAT_R8_UNORM, 0)?;
        let chroma = plane_view(&self.device, &texture, DXGI_FORMAT_R8G8_UNORM, 0)?;

        Ok(Scratch {
            texture,
            luma,
            chroma,
            width,
            height,
        })
    }
}

impl core::fmt::Debug for D3d11Renderer {
    /// Describes the renderer without reaching into COM objects.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("D3d11Renderer")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("tearing", &self.tearing)
            .field("copying", &self.copying)
            .finish_non_exhaustive()
    }
}

/// Returns the DXGI factory that made the device's adapter.
///
/// Taken from the device rather than created afresh so the swap chain lands on the adapter the
/// pictures are already on. A swap chain on a different adapter would mean copying every frame
/// across the bus.
///
/// # Errors
///
/// Returns [`RenderError::Setup`] if the device does not lead back to a factory.
fn factory_for(device: &ID3D11Device) -> Result<IDXGIFactory2, RenderError> {
    let missing = RenderError::Setup {
        reason: "the Direct3D device does not belong to a DXGI adapter",
    };

    // SAFETY: each call is on an interface the one before produced, and all of them outlive
    // this function.
    unsafe {
        let dxgi = device.cast::<IDXGIDevice>().map_err(|_| missing.clone())?;
        let adapter = dxgi.GetAdapter().map_err(|_| missing.clone())?;
        adapter.GetParent::<IDXGIFactory2>().map_err(|_| missing)
    }
}

/// Returns whether this machine will present without waiting for the vertical blank.
///
/// Absent on older Windows builds and on some remote sessions, where the swap chain is built
/// without it and presents at the refresh rate instead.
fn supports_tearing(factory: &IDXGIFactory2) -> bool {
    let Ok(factory) = factory.cast::<IDXGIFactory5>() else {
        return false;
    };

    let mut allowed: u32 = 0;

    // SAFETY: the feature is a plain `BOOL`-sized flag and the size passed matches the local
    // it is written into.
    let queried = unsafe {
        factory.CheckFeatureSupport(
            DXGI_FEATURE_PRESENT_ALLOW_TEARING,
            core::ptr::from_mut(&mut allowed).cast(),
            u32::try_from(size_of::<u32>()).unwrap_or(4),
        )
    };

    queried.is_ok() && allowed != 0
}

/// Holds the swap chain to one frame and returns the object that says when it will take another.
///
/// Returns an invalid handle when the swap chain has no waitable object, which is not an
/// error: it means every frame is presented rather than dropped when the display is behind.
fn configure_latency(swapchain: &IDXGISwapChain1) -> HANDLE {
    let Ok(swapchain) = swapchain.cast::<IDXGISwapChain2>() else {
        return HANDLE::default();
    };

    // SAFETY: both calls are on a swap chain that outlives them. One frame is the shallowest
    // queue Direct3D accepts, and the handle is owned by the swap chain rather than by us.
    unsafe {
        let _ = swapchain.SetMaximumFrameLatency(1);
        swapchain.GetFrameLatencyWaitableObject()
    }
}

/// Builds a render target view onto the swap chain's back buffer.
///
/// # Errors
///
/// Returns [`RenderError::Setup`] if the buffer cannot be reached or the view not created.
fn back_buffer_view(
    device: &ID3D11Device,
    swapchain: &IDXGISwapChain1,
) -> Result<ID3D11RenderTargetView, RenderError> {
    // SAFETY: buffer zero always exists on a created swap chain, and the output is a live
    // local read only on success.
    unsafe {
        let buffer = swapchain
            .GetBuffer::<ID3D11Texture2D>(0)
            .map_err(|_| RenderError::Setup {
                reason: "the swap chain has no back buffer",
            })?;

        let mut view = None;
        device
            .CreateRenderTargetView(&buffer, None, Some(&mut view))
            .map_err(|_| RenderError::Setup {
                reason: "could not create a view onto the back buffer",
            })?;

        view.ok_or(RenderError::Setup {
            reason: "Direct3D reported success but produced no render target view",
        })
    }
}

/// Builds a shader resource view onto one plane of one slice of a texture.
///
/// The plane is chosen by the view's format, which is how Direct3D addresses planar surfaces:
/// `R8_UNORM` is the luma plane and `R8G8_UNORM` the chroma one. The array form is used even
/// for a texture holding one picture, because a decoder that decodes into an array hands back
/// the same texture every time and only the slice differs.
///
/// # Errors
///
/// Returns [`RenderError::Bind`] if the texture was not created to be sampled.
fn plane_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    index: u32,
) -> Result<ID3D11ShaderResourceView, RenderError> {
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: index,
                ArraySize: 1,
            },
        },
    };

    // SAFETY: the description is fully initialised for an array view and the output is a live
    // local read only on success.
    unsafe {
        let mut view = None;
        device
            .CreateShaderResourceView(texture, Some(&desc), Some(&mut view))
            .map_err(|err| RenderError::Bind {
                status: err.code().0,
            })?;

        view.ok_or(RenderError::Bind { status: 0 })
    }
}

/// Creates a sampler that clamps at the edges with the given filter.
///
/// # Errors
///
/// Returns [`RenderError::Setup`] if Direct3D will not create it.
fn sampler(
    device: &ID3D11Device,
    filter: windows::Win32::Graphics::Direct3D11::D3D11_FILTER,
) -> Result<ID3D11SamplerState, RenderError> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: filter,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        MaxLOD: f32::MAX,
        ..Default::default()
    };

    // SAFETY: the description is fully initialised and the output is a live local.
    unsafe {
        let mut sampler = None;
        device
            .CreateSamplerState(&desc, Some(&mut sampler))
            .map_err(|_| RenderError::Setup {
                reason: "could not create a sampler",
            })?;

        sampler.ok_or(RenderError::Setup {
            reason: "Direct3D reported success but produced no sampler",
        })
    }
}

/// Creates the blend state the overlays are drawn with.
///
/// Premultiplied: the bitmaps carry their colour already scaled by their coverage, so the
/// source factor is one rather than the source alpha.
///
/// # Errors
///
/// Returns [`RenderError::Setup`] if Direct3D will not create it.
fn premultiplied_blend(device: &ID3D11Device) -> Result<ID3D11BlendState, RenderError> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: D3D11_BLEND_ONE,
        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };

    // SAFETY: the description is fully initialised and the output is a live local.
    unsafe {
        let mut blend = None;
        device
            .CreateBlendState(&desc, Some(&mut blend))
            .map_err(|_| RenderError::Setup {
                reason: "could not create the overlay blend state",
            })?;

        blend.ok_or(RenderError::Setup {
            reason: "Direct3D reported success but produced no blend state",
        })
    }
}

/// Creates the constant buffer each overlay's rectangle is written into.
///
/// # Errors
///
/// Returns [`RenderError::Setup`] if Direct3D will not create it.
fn placement_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer, RenderError> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: u32::try_from(size_of::<[f32; 4]>()).unwrap_or(16),
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };

    // SAFETY: the description is fully initialised and the output is a live local.
    unsafe {
        let mut buffer = None;
        device
            .CreateBuffer(&desc, None, Some(&mut buffer))
            .map_err(|_| RenderError::Setup {
                reason: "could not create the overlay placement buffer",
            })?;

        buffer.ok_or(RenderError::Setup {
            reason: "Direct3D reported success but produced no placement buffer",
        })
    }
}

/// Compiles and creates one vertex shader.
///
/// # Errors
///
/// Returns [`RenderError::Shader`] if it will not compile and [`RenderError::Setup`] if
/// Direct3D will not create it.
fn vertex_shader(device: &ID3D11Device, entry: &str) -> Result<ID3D11VertexShader, RenderError> {
    let code = compile(entry, "vs_5_0")?;

    // SAFETY: the blob holds compiled vertex bytecode and the output is a live local.
    unsafe {
        let mut shader = None;
        device
            .CreateVertexShader(&code, None, Some(&mut shader))
            .map_err(|_| RenderError::Setup {
                reason: "could not create a vertex shader",
            })?;

        shader.ok_or(RenderError::Setup {
            reason: "Direct3D reported success but produced no vertex shader",
        })
    }
}

/// Compiles and creates one pixel shader.
///
/// # Errors
///
/// Returns [`RenderError::Shader`] if it will not compile and [`RenderError::Setup`] if
/// Direct3D will not create it.
fn pixel_shader(device: &ID3D11Device, entry: &str) -> Result<ID3D11PixelShader, RenderError> {
    let code = compile(entry, "ps_5_0")?;

    // SAFETY: the blob holds compiled pixel bytecode and the output is a live local.
    unsafe {
        let mut shader = None;
        device
            .CreatePixelShader(&code, None, Some(&mut shader))
            .map_err(|_| RenderError::Setup {
                reason: "could not create a pixel shader",
            })?;

        shader.ok_or(RenderError::Setup {
            reason: "Direct3D reported success but produced no pixel shader",
        })
    }
}

/// Compiles one entry point of the shader source.
///
/// # Errors
///
/// Returns [`RenderError::Shader`] with whatever the compiler said.
fn compile(entry: &str, target: &str) -> Result<Vec<u8>, RenderError> {
    let entry = std::ffi::CString::new(entry).expect("entry point names hold no interior nul");
    let target = std::ffi::CString::new(target).expect("shader targets hold no interior nul");

    let mut code = None;
    let mut errors = None;

    // SAFETY: the source is a live slice for the duration of the call, both name pointers are
    // nul-terminated and live, and the outputs are live locals.
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
        return Err(RenderError::Shader {
            message: compiler_message(errors.as_ref()),
        });
    }

    let code = code.ok_or(RenderError::Shader {
        message: "the shader compiler reported success but produced no bytecode".to_owned(),
    })?;

    // SAFETY: the blob is alive and reports its own buffer and length.
    let bytes = unsafe {
        core::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };

    Ok(bytes.to_vec())
}

/// Reads what the shader compiler said, if it said anything.
fn compiler_message(errors: Option<&windows::Win32::Graphics::Direct3D::ID3DBlob>) -> String {
    let Some(errors) = errors else {
        return "the shader compiler gave no reason".to_owned();
    };

    // SAFETY: the blob is alive and reports its own buffer and length.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            errors.GetBufferPointer().cast::<u8>(),
            errors.GetBufferSize(),
        )
    };

    String::from_utf8_lossy(bytes)
        .trim_end_matches('\0')
        .trim()
        .to_owned()
}
