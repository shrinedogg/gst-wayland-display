use smithay::{
    backend::{allocator::Buffer, allocator::dmabuf::Dmabuf, renderer::ImportDma},
    delegate_dmabuf,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};

use crate::comp::State;

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        match self.renderer.import_dmabuf(&dmabuf, None) {
            Ok(_) => {
                let _ = notifier.successful::<State>();
            }
            Err(err) => {
                // Diagnostics for the gamescope EGL_BAD_ALLOC rejection: log the
                // full buffer description so the failing format/modifier/plane
                // layout can be compared against a known-good import.
                tracing::warn!(
                    width = dmabuf.width(),
                    height = dmabuf.height(),
                    format = ?dmabuf.format(),
                    planes = dmabuf.num_planes(),
                    offsets = ?dmabuf.offsets().collect::<Vec<_>>(),
                    strides = ?dmabuf.strides().collect::<Vec<_>>(),
                    handles = ?dmabuf.handles().collect::<Vec<_>>(),
                    error = ?err,
                    "dmabuf test-import failed; rejecting client buffer"
                );
                notifier.failed();
            }
        }
    }
}

delegate_dmabuf!(State);
