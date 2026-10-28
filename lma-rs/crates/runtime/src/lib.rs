//! Runtime for the recompiled game: guest memory, kernel/XAM high-level emulation, VFS.

/// The git commit this binary was built from (`git describe --dirty`; see build.rs).
pub const BUILD: &str = env!("LMA_BUILD");

#[cfg(feature = "tracy")]
#[doc(hidden)]
pub use tracy_client;

/// A Tracy zone for the rest of the enclosing block (`zone!("flush")`). Without the `tracy`
/// feature it expands to nothing.
#[macro_export]
macro_rules! zone {
    ($name:literal) => {
        #[cfg(feature = "tracy")]
        let _zone = $crate::tracy_client::Client::start().span($crate::tracy_client::span_location!($name), 0);
    };
}

/// A Tracy frame mark (the guest presented a frame). Without the `tracy` feature it does nothing.
#[inline(always)]
pub fn frame_mark() {
    #[cfg(feature = "tracy")]
    tracy_client::Client::start().frame_mark();
}

pub mod backend;
pub mod audio;
pub mod backchain;
pub mod content;
pub mod cp_thread;
// moved to the `guest` crate (the interface recompiled code depends on); same paths as before
pub use guest::{cpu, mem, rec, vmx};
pub use guest::mmio;
pub mod exports;
pub mod fasthash;
pub mod fmt;
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
pub(crate) mod par;
pub mod gpu;
pub mod hooks;
#[cfg(feature = "wgpu")]
pub mod gpu_wgpu;
pub mod input;
pub mod journal;
pub mod machine;
pub mod mods;
pub mod native_audio;
pub mod prof;
pub mod raster;
pub mod shader;
pub mod shader_exec;
pub mod shader_wgsl;
pub mod tex_override;
pub mod threaded;
pub mod threads;
pub mod vfs;
pub mod xact;
pub mod xact_cache;
pub mod xma;
