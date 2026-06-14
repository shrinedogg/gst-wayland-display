use crate::{
    comp::State,
    wayland::protocols::color_management::{
        ColorManagementHandler, HdrMetadata, delegate_color_management,
    },
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

impl ColorManagementHandler for State {
    fn surface_color_state(&mut self, surface: &WlSurface, meta: Option<HdrMetadata>) {
        match meta {
            Some(meta) => {
                tracing::debug!(?meta, "color-management: surface image description set");
                self.surface_hdr_metadata.insert(surface.clone(), meta);
            }
            None => {
                self.surface_hdr_metadata.remove(surface);
            }
        }
    }
}

delegate_color_management!(State);
