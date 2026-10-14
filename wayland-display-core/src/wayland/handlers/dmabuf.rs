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

                // A/B probe: is THIS buffer broken, or is the renderer/display
                // state broken? fstat the client fd, then allocate our own GBM
                // buffer on the same node with the client's exact
                // format+modifier and test-import it through the same renderer.
                {
                    use smithay::backend::allocator::Allocator;
                    use smithay::backend::allocator::dmabuf::DmabufAllocator;
                    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags};
                    use std::os::fd::AsRawFd;
                    use std::os::unix::fs::MetadataExt;

                    for h in dmabuf.handles() {
                        let fd = h.as_raw_fd();
                        match std::fs::metadata(format!("/proc/self/fd/{fd}")) {
                            Ok(md) => tracing::warn!(fd, size = md.size(), "client dmabuf fstat"),
                            Err(e) => tracing::warn!(fd, error = ?e, "client dmabuf fstat failed"),
                        }
                    }

                    if let Some(node) = self.render_node {
                        if let Some(gbm) = crate::utils::allocator::new_gbm_device(node) {
                            let allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
                            let mut dma_allocator = DmabufAllocator(allocator);
                            let mods = [dmabuf.format().modifier];
                            match dma_allocator.create_buffer(
                                dmabuf.width(),
                                dmabuf.height(),
                                dmabuf.format().code,
                                &mods,
                            ) {
                                Ok(buf) => {
                                    let r = self.renderer.import_dmabuf(&buf, None);
                                    tracing::warn!(
                                        ok = r.is_ok(),
                                        error = ?r.err(),
                                        "A/B probe: self-allocated GBM buffer (client format+modifier) import"
                                    );
                                }
                                Err(e) => tracing::warn!(
                                    error = ?e,
                                    "A/B probe: GBM create_buffer with client modifier failed"
                                ),
                            }
                        }
                    }
                }

                notifier.failed();
            }
        }
    }
}

delegate_dmabuf!(State);
