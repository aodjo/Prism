//! Metal renderer for decoded pictures.
//!
//! Decoded frames arrive as IOSurface-backed `CVPixelBuffer`s in NV12. Rather than
//! reading them back and converting on the CPU — three megabytes a frame at 1080p, every
//! frame — the two planes are bound directly as Metal textures through a
//! `CVMetalTextureCache` and converted in a fragment shader.
//!
//! The shader is compiled once at startup from source held in this file. Shipping the
//! source rather than a precompiled library keeps the renderer independent of the build
//! machine's Metal toolchain.

use core::ptr::{NonNull, null_mut};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVImageBuffer, CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBuffer,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth,
};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder,
    MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLLoadAction,
    MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLStoreAction, MTLTexture,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use crate::render::RenderError;
use crate::render::overlay::TextOverlay;

/// The shader that turns an NV12 picture into RGB.
///
/// A full-screen triangle rather than a quad, so there is no vertex buffer to manage and
/// no seam down the diagonal. The colour conversion is BT.709 video range, which is what
/// VideoToolbox produces for the resolutions this pipeline uses.
const SHADER_SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexOut {
    float4 position [[position]];
    float2 uv;
};

vertex VertexOut prism_vertex(uint vid [[vertex_id]]) {
    const float2 corners[3] = { float2(-1.0, -3.0), float2(-1.0, 1.0), float2(3.0, 1.0) };
    float2 p = corners[vid];

    VertexOut out;
    out.position = float4(p, 0.0, 1.0);
    out.uv = float2((p.x + 1.0) * 0.5, 1.0 - (p.y + 1.0) * 0.5);
    return out;
}

fragment float4 prism_fragment(VertexOut in [[stage_in]],
                               texture2d<float> luma [[texture(0)]],
                               texture2d<float> chroma [[texture(1)]]) {
    constexpr sampler bilinear(filter::linear, address::clamp_to_edge);

    float y = luma.sample(bilinear, in.uv).r;
    float2 cbcr = chroma.sample(bilinear, in.uv).rg;

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

struct OverlayVertexOut {
    float4 position [[position]];
    float2 uv;
};

// `rect` is (left, top, width, height) in normalised device coordinates, where y grows
// upwards. No vertical flip: a CoreGraphics bitmap context draws with its origin at the
// bottom left, but lays its rows out top down in memory, so row zero is already the top
// of the image and samples straight through.
vertex OverlayVertexOut prism_overlay_vertex(uint vid [[vertex_id]],
                                             constant float4 &rect [[buffer(0)]]) {
    const float2 corners[6] = {
        float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
        float2(1.0, 0.0), float2(1.0, 1.0), float2(0.0, 1.0)
    };
    float2 c = corners[vid];

    OverlayVertexOut out;
    out.position = float4(rect.x + c.x * rect.z, rect.y - c.y * rect.w, 0.0, 1.0);
    out.uv = c;
    return out;
}

fragment float4 prism_overlay_fragment(OverlayVertexOut in [[stage_in]],
                                       texture2d<float> glyphs [[texture(0)]]) {
    constexpr sampler nearest(filter::nearest, address::clamp_to_edge);
    return glyphs.sample(nearest, in.uv);
}
"#;

/// Draws decoded pictures into a Metal texture.
#[derive(Debug)]
pub struct MetalRenderer {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    overlay_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    cache: CFRetained<CVMetalTextureCache>,
}

impl MetalRenderer {
    /// Creates a renderer on the system's default GPU.
    ///
    /// The pipeline is built for `format`, which must match the texture the renderer will
    /// be asked to draw into — a layer's drawable format, usually BGRA8.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if there is no GPU or a Metal object cannot be
    /// created, and [`RenderError::Shader`] if the shader fails to compile.
    pub fn new(format: MTLPixelFormat) -> Result<Self, RenderError> {
        let device = MTLCreateSystemDefaultDevice().ok_or(RenderError::Setup {
            reason: "no Metal device is available",
        })?;

        let queue = device.newCommandQueue().ok_or(RenderError::Setup {
            reason: "could not create a command queue",
        })?;

        let source = NSString::from_str(SHADER_SOURCE);
        let library = device
            .newLibraryWithSource_options_error(&source, None)
            .map_err(|err| RenderError::Shader {
                message: err.localizedDescription().to_string(),
            })?;

        let vertex = library
            .newFunctionWithName(&NSString::from_str("prism_vertex"))
            .ok_or(RenderError::Setup {
                reason: "the vertex function is missing",
            })?;
        let fragment = library
            .newFunctionWithName(&NSString::from_str("prism_fragment"))
            .ok_or(RenderError::Setup {
                reason: "the fragment function is missing",
            })?;

        let descriptor = MTLRenderPipelineDescriptor::new();
        descriptor.setVertexFunction(Some(&vertex));
        descriptor.setFragmentFunction(Some(&fragment));
        // SAFETY: attachment zero always exists on a fresh pipeline descriptor.
        unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(format);

        let pipeline = device
            .newRenderPipelineStateWithDescriptor_error(&descriptor)
            .map_err(|err| RenderError::Shader {
                message: err.localizedDescription().to_string(),
            })?;

        let overlay_pipeline = build_overlay_pipeline(&device, &library, format)?;

        let mut raw: *mut CVMetalTextureCache = null_mut();

        // SAFETY: the device outlives the call and CoreVideo writes a retained cache into
        // `raw` on success.
        let status = unsafe {
            CVMetalTextureCache::create(None, None, &device, None, NonNull::from(&mut raw))
        };

        if status != 0 || raw.is_null() {
            return Err(RenderError::Bind { status });
        }

        // SAFETY: CoreVideo created the cache, so ownership transfers here.
        let cache = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };

        Ok(Self {
            device,
            queue,
            pipeline,
            overlay_pipeline,
            cache,
        })
    }

    /// Returns the GPU this renderer runs on.
    #[must_use]
    pub fn device(&self) -> &ProtocolObject<dyn MTLDevice> {
        &self.device
    }

    /// Prepares a layer to receive frames from this renderer.
    ///
    /// Display sync is turned off and the drawable count held at two. Both trade a little
    /// smoothness for latency, which is the trade this whole project makes: waiting for
    /// the next vertical blank to hand over a frame that is already finished adds up to a
    /// frame of delay, and a third drawable adds another.
    pub fn configure_layer(&self, layer: &CAMetalLayer, width: usize, height: usize) {
        layer.setDevice(Some(&self.device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(true);
        layer.setDisplaySyncEnabled(false);
        layer.setMaximumDrawableCount(2);
        layer.setDrawableSize(objc2_core_foundation::CGSize {
            width: width as f64,
            height: height as f64,
        });
    }

    /// Draws one picture into `target` and waits for the GPU to finish.
    ///
    /// Waiting is for offscreen work such as tests. The live path uses [`Self::present`],
    /// which does not block.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] if either plane cannot be bound as a texture, and
    /// [`RenderError::Setup`] if Metal refuses a command buffer or encoder.
    pub fn draw(
        &mut self,
        picture: &CVPixelBuffer,
        target: &ProtocolObject<dyn MTLTexture>,
    ) -> Result<(), RenderError> {
        let command_buffer = self.record(picture, target, None)?;
        command_buffer.commit();
        command_buffer.waitUntilCompleted();

        Ok(())
    }

    /// Draws one picture into a layer's next drawable and presents it.
    ///
    /// Does not wait for the GPU. Blocking here would serialise the decoder against the
    /// display, which costs a frame for no benefit — the drawable is already scheduled
    /// and the next picture can start while this one is still being drawn.
    ///
    /// Returns `false` when the layer had no drawable available, which happens when the
    /// display is behind; the frame is dropped rather than queued because a frame shown
    /// late is worse than one not shown at all.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] if either plane cannot be bound as a texture, and
    /// [`RenderError::Setup`] if Metal refuses a command buffer or encoder.
    pub fn present(
        &mut self,
        picture: &CVPixelBuffer,
        layer: &CAMetalLayer,
        overlay: Option<&TextOverlay>,
    ) -> Result<bool, RenderError> {
        let Some(drawable) = layer.nextDrawable() else {
            return Ok(false);
        };

        let target = drawable.texture();
        let command_buffer = self.record(picture, &target, overlay)?;

        command_buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
        command_buffer.commit();

        Ok(true)
    }

    /// Records the conversion pass into a fresh command buffer.
    ///
    /// The bound plane textures are dropped when this returns, which is safe because
    /// Metal retains everything a committed command buffer refers to.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] if either plane cannot be bound, and
    /// [`RenderError::Setup`] if Metal refuses a command buffer or encoder.
    fn record(
        &mut self,
        picture: &CVPixelBuffer,
        target: &ProtocolObject<dyn MTLTexture>,
        overlay: Option<&TextOverlay>,
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, RenderError> {
        let (width, height) = (
            CVPixelBufferGetWidth(picture),
            CVPixelBufferGetHeight(picture),
        );

        let luma = self.bind_plane(picture, MTLPixelFormat::R8Unorm, width, height, 0)?;
        let chroma =
            self.bind_plane(picture, MTLPixelFormat::RG8Unorm, width / 2, height / 2, 1)?;

        let luma_texture = CVMetalTextureGetTexture(&luma).ok_or(RenderError::Setup {
            reason: "the luma plane has no texture",
        })?;
        let chroma_texture = CVMetalTextureGetTexture(&chroma).ok_or(RenderError::Setup {
            reason: "the chroma plane has no texture",
        })?;

        let descriptor = MTLRenderPassDescriptor::renderPassDescriptor();
        // SAFETY: attachment zero always exists on a fresh render pass descriptor.
        let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(target));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        });

        let command_buffer = self.queue.commandBuffer().ok_or(RenderError::Setup {
            reason: "could not create a command buffer",
        })?;
        let encoder = command_buffer
            .renderCommandEncoderWithDescriptor(&descriptor)
            .ok_or(RenderError::Setup {
                reason: "could not create a render encoder",
            })?;

        encoder.setRenderPipelineState(&self.pipeline);
        // SAFETY: both textures outlive the encoder, and the pipeline draws exactly the
        // three vertices its vertex function generates.
        unsafe {
            encoder.setFragmentTexture_atIndex(Some(&luma_texture), 0);
            encoder.setFragmentTexture_atIndex(Some(&chroma_texture), 1);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
        }

        if let Some(overlay) = overlay {
            let rect = overlay_rect(overlay, target.width(), target.height());

            encoder.setRenderPipelineState(&self.overlay_pipeline);
            // SAFETY: the rectangle is four floats, matching the shader's `float4`, and
            // the overlay texture outlives the encoder.
            unsafe {
                encoder.setVertexBytes_length_atIndex(
                    NonNull::from(&rect).cast(),
                    core::mem::size_of_val(&rect),
                    0,
                );
                encoder.setFragmentTexture_atIndex(Some(overlay.texture()), 0);
                encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 6);
            }
        }

        encoder.endEncoding();

        Ok(command_buffer)
    }

    /// Binds one plane of a picture as a Metal texture without copying it.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Bind`] with the CoreVideo status if the plane cannot be
    /// bound, which happens when the picture is not IOSurface backed.
    fn bind_plane(
        &self,
        picture: &CVPixelBuffer,
        format: MTLPixelFormat,
        width: usize,
        height: usize,
        plane: usize,
    ) -> Result<CFRetained<CVMetalTexture>, RenderError> {
        let mut raw: *mut CVMetalTexture = null_mut();

        // SAFETY: the cache and picture are alive, the plane index is within an NV12
        // buffer's two planes, and CoreVideo writes a retained texture into `raw`.
        let status = unsafe {
            let image = &*(core::ptr::from_ref(picture).cast::<CVImageBuffer>());
            CVMetalTextureCache::create_texture_from_image(
                None,
                &self.cache,
                image,
                None,
                format,
                width,
                height,
                plane,
                NonNull::from(&mut raw),
            )
        };

        if status != 0 || raw.is_null() {
            return Err(RenderError::Bind { status });
        }

        // SAFETY: CoreVideo created the texture, so ownership transfers here.
        Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) })
    }
}

/// Builds the pipeline that blends the statistics overlay over the picture.
///
/// Blending is premultiplied: the overlay bitmap is drawn by CoreGraphics with
/// premultiplied alpha, so the source factor is one rather than the source alpha.
///
/// # Errors
///
/// Returns [`RenderError::Setup`] if either shader function is missing, and
/// [`RenderError::Shader`] if Metal will not build the pipeline.
fn build_overlay_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    format: MTLPixelFormat,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, RenderError> {
    let vertex = library
        .newFunctionWithName(&NSString::from_str("prism_overlay_vertex"))
        .ok_or(RenderError::Setup {
            reason: "the overlay vertex function is missing",
        })?;
    let fragment = library
        .newFunctionWithName(&NSString::from_str("prism_overlay_fragment"))
        .ok_or(RenderError::Setup {
            reason: "the overlay fragment function is missing",
        })?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vertex));
    descriptor.setFragmentFunction(Some(&fragment));

    // SAFETY: attachment zero always exists on a fresh pipeline descriptor.
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(format);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|err| RenderError::Shader {
            message: err.localizedDescription().to_string(),
        })
}

/// Returns where the overlay sits on the target, in normalised device coordinates.
///
/// Pinned to the top left at its natural pixel size, so the text stays legible whatever
/// the window is scaled to rather than stretching with it.
fn overlay_rect(overlay: &TextOverlay, target_width: usize, target_height: usize) -> [f32; 4] {
    let margin_x = 16.0 / target_width.max(1) as f32;
    let margin_y = 16.0 / target_height.max(1) as f32;

    let width = (overlay.width() as f32 / target_width.max(1) as f32) * 2.0;
    let height = (overlay.height() as f32 / target_height.max(1) as f32) * 2.0;

    [-1.0 + margin_x * 2.0, 1.0 - margin_y * 2.0, width, height]
}
