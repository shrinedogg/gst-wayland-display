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

                // Retry matrix (2026-10-15): rebuild the client dmabuf with
                // alternate stride/height/modifier and re-import. gamescope's
                // real 3024x1964 buffer is 25,165,824 bytes = 12288*2048 but
                // reports stride 12096, so the declared layout may not match
                // the driver's actual allocation.
                {
                    use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
                    use smithay::backend::allocator::Modifier;
                    use std::os::fd::AsRawFd;

                    let w = dmabuf.width();
                    let h = dmabuf.height();
                    let code = dmabuf.format().code;
                    let bpp4 = w * 4;
                    let round512 = |v: u32| (v + 511) & !511;
                    let mut strides = vec![dmabuf.strides().next().unwrap_or(0)];
                    for c in [round512(bpp4), round512(strides[0]), 12288] {
                        if c > 0 && !strides.contains(&c) {
                            strides.push(c);
                        }
                    }
                    let heights = [h, (h + 127) & !127];
                    let mods = [dmabuf.format().modifier, Modifier::Linear];

                    for &stride in &strides {
                        for &height in &heights {
                            for &md in &mods {
                                let mut b = Dmabuf::builder((w as i32, height as i32), code, md, DmabufFlags::empty());
                                let mut ok = true;
                                for (i, hdl) in dmabuf.handles().enumerate() {
                                    let off = dmabuf.offsets().nth(i).unwrap_or(0);
                                    // dup via /proc/self/fd (no libc dep); needs
                                    // read+write on dma-buf fds
                                    match std::fs::OpenOptions::new()
                                        .read(true)
                                        .write(true)
                                        .open(format!("/proc/self/fd/{}", hdl.as_raw_fd()))
                                    {
                                        Ok(f) => {
                                            let owned: std::os::fd::OwnedFd = f.into();
                                            if !b.add_plane(owned, i as u32, off, if i == 0 { stride } else { dmabuf.strides().nth(i).unwrap_or(stride) }) {
                                                tracing::warn!(stride, "retry: add_plane failed");
                                                ok = false;
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(fd = hdl.as_raw_fd(), error = ?e, "retry: fd dup failed");
                                            ok = false;
                                            break;
                                        }
                                    }
                                }
                                if !ok {
                                    continue;
                                }
                                if let Some(cand) = b.build() {
                                    let r = self.renderer.import_dmabuf(&cand, None);
                                    tracing::warn!(
                                        stride,
                                        height,
                                        modifier = ?md,
                                        ok = r.is_ok(),
                                        "retry import with alternate layout"
                                    );
                                }
                            }
                        }
                    }
                }

                // DEBUG_ACCEPT (2026-10-15): report success to the client
                // even though the import failed. The buffer will render
                // garbage/black, but the client (gamescope) stays alive
                // instead of crashlooping, so its dmabuf fds can be
                // introspected live. REVERT after diagnosis.
                tracing::warn!("DEBUG_ACCEPT: reporting import success despite failure");
                let _ = notifier.successful::<State>();
            }
        }
    }
}

delegate_dmabuf!(State);
