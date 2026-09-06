//! Drawing decoded pictures.
//!
//! The decoder hands over pictures in the GPU's own memory, and the renderer's job is to
//! get them onto the screen without ever copying them through system memory. On macOS a
//! decoded `CVPixelBuffer` is IOSurface backed, so it can be bound as a Metal texture
//! directly; the colour conversion from NV12 to RGB happens in a fragment shader rather
//! than on the CPU.

#[cfg(target_os = "macos")]
pub mod metal;

/// Reason a picture could not be drawn.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// No GPU was available, or it refused to create something the renderer needs.
    #[error("could not set up the renderer: {reason}")]
    Setup {
        /// What went wrong.
        reason: &'static str,
    },

    /// The shader source failed to compile.
    #[error("shader compilation failed: {message}")]
    Shader {
        /// What the compiler reported.
        message: String,
    },

    /// A decoded picture could not be bound as a texture.
    #[error("could not bind the picture as a texture (status {status})")]
    Bind {
        /// Platform status code.
        status: i32,
    },
}
