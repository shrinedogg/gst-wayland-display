// SPDX-License-Identifier: GPL-3.0-only
//
// Minimal server-side implementation of the `color-management-v1` staging
// protocol (wp_color_manager_v1, version 3). Smithay does not implement this
// yet, so -- following the same codegen + delegate pattern as `wl_drm` in this
// crate -- we generate the bindings from the bundled XML and implement just
// enough Dispatch to:
//   * advertise the features an HDR client (gamescope) needs, and
//   * capture the HDR image-description metadata a client attaches to a surface
//     (transfer function, primaries, mastering display, max-CLL/FALL).
//
// The captured metadata is handed to `ColorManagementHandler` so the compositor
// can forward BT.2020 + PQ + HDR10 static metadata downstream (GStreamer caps ->
// Wolf -> Moonlight). Color *conversion* (the sRGB->BT.2020 shader) is separate
// (Phase 1.3); this module only does protocol + capture.
pub use generated::*;

mod generated {
    use smithay::reexports::wayland_server::{self, protocol::*};

    pub mod __interfaces {
        use smithay::reexports::wayland_server::protocol::__interfaces::*;
        use wayland_backend;
        wayland_scanner::generate_interfaces!("resources/protocols/color-management-v1.xml");
    }

    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("resources/protocols/color-management-v1.xml");
}

use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
    backend::GlobalId,
    protocol::wl_surface::WlSurface,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

pub use wp_color_manager_v1::{Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1};
pub use wp_color_management_output_v1::WpColorManagementOutputV1;
pub use wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1;
pub use wp_color_management_surface_v1::WpColorManagementSurfaceV1;
pub use wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1;
pub use wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1;
pub use wp_image_description_info_v1::WpImageDescriptionInfoV1;
pub use wp_image_description_reference_v1::WpImageDescriptionReferenceV1;
pub use wp_image_description_v1::WpImageDescriptionV1;

/// Monotonic image-description identity counter (protocol requires unique,
/// never-recycled, non-zero ids).
static IMAGE_DESC_IDENTITY: AtomicU64 = AtomicU64::new(1);

fn next_identity() -> u64 {
    IMAGE_DESC_IDENTITY.fetch_add(1, Ordering::Relaxed)
}

/// HDR static metadata captured from a client's parametric image description.
/// Chromaticities are CIE 1931 xy * 1_000_000; luminances follow the protocol's
/// scaling (min_lum * 10_000, max_lum unscaled).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HdrMetadata {
    pub transfer: Option<TransferFunction>,
    pub primaries: Option<Primaries>,
    /// r_x,r_y,g_x,g_y,b_x,b_y,w_x,w_y (xy * 1e6)
    pub mastering_display_primaries: Option<[i32; 8]>,
    pub mastering_min_lum: Option<u32>,
    pub mastering_max_lum: Option<u32>,
    pub primary_min_lum: Option<u32>,
    pub primary_max_lum: Option<u32>,
    pub reference_lum: Option<u32>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

impl HdrMetadata {
    /// True if this describes an HDR (PQ / BT.2020) image.
    pub fn is_hdr(&self) -> bool {
        matches!(self.transfer, Some(TransferFunction::St2084Pq))
            || matches!(self.primaries, Some(Primaries::Bt2020))
    }
}

/// Per-`wp_image_description_creator_params_v1` accumulated state.
#[derive(Default)]
pub struct ParamsData(pub Arc<Mutex<HdrMetadata>>);

/// Immutable data backing a `wp_image_description_v1`.
#[derive(Clone)]
pub struct ImageDescriptionData {
    pub meta: HdrMetadata,
    pub allow_info: bool,
    pub identity: u64,
}

/// Backing data for surface/feedback objects: the associated wl_surface.
#[derive(Clone)]
pub struct CmSurfaceData {
    pub surface: WlSurface,
}

/// ZST carrying all the protocol Dispatch impls (generic over the compositor
/// state `D`), mirroring `WlDrmState`.
pub struct ColorMgmtState;

/// Aggregated bound so each Dispatch impl below can require a single trait and
/// the blanket impl wires it to `State` automatically.
pub trait ColorMgmt:
    GlobalDispatch<WpColorManagerV1, ()>
    + Dispatch<WpColorManagerV1, ()>
    + Dispatch<WpColorManagementOutputV1, ()>
    + Dispatch<WpColorManagementSurfaceV1, CmSurfaceData>
    + Dispatch<WpColorManagementSurfaceFeedbackV1, CmSurfaceData>
    + Dispatch<WpImageDescriptionCreatorIccV1, ()>
    + Dispatch<WpImageDescriptionCreatorParamsV1, ParamsData>
    + Dispatch<WpImageDescriptionV1, ImageDescriptionData>
    + Dispatch<WpImageDescriptionInfoV1, ImageDescriptionData>
    + Dispatch<WpImageDescriptionReferenceV1, ()>
    + ColorManagementHandler
    + 'static
{
}

impl<T> ColorMgmt for T where
    T: GlobalDispatch<WpColorManagerV1, ()>
        + Dispatch<WpColorManagerV1, ()>
        + Dispatch<WpColorManagementOutputV1, ()>
        + Dispatch<WpColorManagementSurfaceV1, CmSurfaceData>
        + Dispatch<WpColorManagementSurfaceFeedbackV1, CmSurfaceData>
        + Dispatch<WpImageDescriptionCreatorIccV1, ()>
        + Dispatch<WpImageDescriptionCreatorParamsV1, ParamsData>
        + Dispatch<WpImageDescriptionV1, ImageDescriptionData>
        + Dispatch<WpImageDescriptionInfoV1, ImageDescriptionData>
        + Dispatch<WpImageDescriptionReferenceV1, ()>
        + ColorManagementHandler
        + 'static
{
}

/// Implemented by the compositor state to receive captured surface color state.
pub trait ColorManagementHandler {
    /// Called when a client sets (or unsets, `None`) an image description on a
    /// surface. Metadata is applied immediately (the prototype does not honor
    /// the protocol's double-buffering-on-commit semantics).
    fn surface_color_state(&mut self, surface: &WlSurface, meta: Option<HdrMetadata>);
}

/// Send the appropriate "ready" event for the negotiated interface version.
fn send_ready(desc: &WpImageDescriptionV1, identity: u64) {
    if desc.version() >= 2 {
        desc.ready2((identity >> 32) as u32, (identity & 0xffff_ffff) as u32);
    } else {
        desc.ready(identity as u32);
    }
}

impl<D: ColorMgmt> GlobalDispatch<WpColorManagerV1, (), D> for ColorMgmtState {
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let mgr = data_init.init(resource, ());
        // Intents: compositors must support perceptual.
        mgr.supported_intent(RenderIntent::Perceptual);
        // Features we honor (capture-only; conversion is downstream).
        mgr.supported_feature(Feature::Parametric);
        mgr.supported_feature(Feature::SetMasteringDisplayPrimaries);
        mgr.supported_feature(Feature::SetLuminances);
        // Transfer functions: SDR gamma 2.2 + HDR PQ.
        mgr.supported_tf_named(TransferFunction::Gamma22);
        mgr.supported_tf_named(TransferFunction::St2084Pq);
        // Primaries: sRGB + BT.2020.
        mgr.supported_primaries_named(Primaries::Srgb);
        mgr.supported_primaries_named(Primaries::Bt2020);
        mgr.done();
    }
}

impl<D: ColorMgmt> Dispatch<WpColorManagerV1, (), D> for ColorMgmtState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _mgr: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_manager_v1::Request;
        match request {
            Request::GetOutput { id, .. } => {
                data_init.init(id, ());
            }
            Request::GetSurface { id, surface } => {
                data_init.init(id, CmSurfaceData { surface });
            }
            Request::GetSurfaceFeedback { id, surface } => {
                data_init.init(id, CmSurfaceData { surface });
            }
            Request::CreateParametricCreator { obj } => {
                data_init.init(obj, ParamsData::default());
            }
            Request::CreateIccCreator { obj } => {
                // ICC not advertised; init then it will be unusable.
                data_init.init(obj, ());
            }
            Request::CreateWindowsScrgb { image_description }
            | Request::CreateWindowsBt2100 { image_description } => {
                let meta = HdrMetadata {
                    transfer: Some(TransferFunction::St2084Pq),
                    primaries: Some(Primaries::Bt2020),
                    ..Default::default()
                };
                let identity = next_identity();
                let desc = data_init.init(
                    image_description,
                    ImageDescriptionData {
                        meta,
                        allow_info: false,
                        identity,
                    },
                );
                send_ready(&desc, identity);
            }
            Request::GetImageDescription {
                image_description, ..
            } => {
                let identity = next_identity();
                let desc = data_init.init(
                    image_description,
                    ImageDescriptionData {
                        meta: HdrMetadata::default(),
                        allow_info: true,
                        identity,
                    },
                );
                send_ready(&desc, identity);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: ColorMgmt> Dispatch<WpImageDescriptionCreatorParamsV1, ParamsData, D> for ColorMgmtState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _obj: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        data: &ParamsData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_image_description_creator_params_v1::Request;
        match request {
            Request::SetTfNamed { tf } => {
                if let WEnum::Value(tf) = tf {
                    data.0.lock().unwrap().transfer = Some(tf);
                }
            }
            Request::SetPrimariesNamed { primaries } => {
                if let WEnum::Value(p) = primaries {
                    data.0.lock().unwrap().primaries = Some(p);
                }
            }
            Request::SetMasteringDisplayPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                data.0.lock().unwrap().mastering_display_primaries =
                    Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
            }
            Request::SetMasteringLuminance { min_lum, max_lum } => {
                let mut m = data.0.lock().unwrap();
                m.mastering_min_lum = Some(min_lum);
                m.mastering_max_lum = Some(max_lum);
            }
            Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                let mut m = data.0.lock().unwrap();
                m.primary_min_lum = Some(min_lum);
                m.primary_max_lum = Some(max_lum);
                m.reference_lum = Some(reference_lum);
            }
            Request::SetMaxCll { max_cll } => {
                data.0.lock().unwrap().max_cll = Some(max_cll);
            }
            Request::SetMaxFall { max_fall } => {
                data.0.lock().unwrap().max_fall = Some(max_fall);
            }
            Request::Create { image_description } => {
                let meta = *data.0.lock().unwrap();
                let identity = next_identity();
                let desc = data_init.init(
                    image_description,
                    ImageDescriptionData {
                        meta,
                        allow_info: false,
                        identity,
                    },
                );
                send_ready(&desc, identity);
            }
            _ => {}
        }
    }
}

impl<D: ColorMgmt> Dispatch<WpImageDescriptionV1, ImageDescriptionData, D> for ColorMgmtState {
    fn request(
        _state: &mut D,
        _client: &Client,
        desc: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        data: &ImageDescriptionData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_image_description_v1::Request;
        match request {
            Request::GetInformation { information } => {
                if !data.allow_info {
                    desc.post_error(
                        wp_image_description_v1::Error::NoInformation,
                        "get_information not allowed for this image description",
                    );
                    return;
                }
                let info = data_init.init(information, data.clone());
                send_image_description_info(&info, &data.meta);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

/// Emit the info events for an image description, then `done` (which destroys
/// the info object per the protocol).
fn send_image_description_info(info: &WpImageDescriptionInfoV1, meta: &HdrMetadata) {
    if let Some(p) = meta.mastering_display_primaries {
        info.primaries(p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
    }
    if let Some(prim) = meta.primaries {
        info.primaries_named(prim);
    }
    if let Some(tf) = meta.transfer {
        info.tf_named(tf);
    }
    info.done();
}

impl<D: ColorMgmt> Dispatch<WpImageDescriptionInfoV1, ImageDescriptionData, D> for ColorMgmtState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _info: &WpImageDescriptionInfoV1,
        _request: wp_image_description_info_v1::Request,
        _data: &ImageDescriptionData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // wp_image_description_info_v1 has no client requests.
    }
}

impl<D: ColorMgmt> Dispatch<WpColorManagementSurfaceV1, CmSurfaceData, D> for ColorMgmtState {
    fn request(
        state: &mut D,
        _client: &Client,
        _obj: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        data: &CmSurfaceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_surface_v1::Request;
        match request {
            Request::SetImageDescription {
                image_description, ..
            } => {
                let meta = image_description
                    .data::<ImageDescriptionData>()
                    .map(|d| d.meta);
                state.surface_color_state(&data.surface, meta);
            }
            Request::UnsetImageDescription => {
                state.surface_color_state(&data.surface, None);
            }
            Request::Destroy => {
                state.surface_color_state(&data.surface, None);
            }
            _ => {}
        }
    }
}

impl<D: ColorMgmt> Dispatch<WpColorManagementSurfaceFeedbackV1, CmSurfaceData, D> for ColorMgmtState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _obj: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        _data: &CmSurfaceData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_surface_feedback_v1::Request;
        // Advertise our preferred encoding as BT.2020 + PQ.
        let preferred = HdrMetadata {
            transfer: Some(TransferFunction::St2084Pq),
            primaries: Some(Primaries::Bt2020),
            ..Default::default()
        };
        match request {
            Request::GetPreferred { image_description }
            | Request::GetPreferredParametric { image_description } => {
                let identity = next_identity();
                let desc = data_init.init(
                    image_description,
                    ImageDescriptionData {
                        meta: preferred,
                        allow_info: true,
                        identity,
                    },
                );
                send_ready(&desc, identity);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: ColorMgmt> Dispatch<WpColorManagementOutputV1, (), D> for ColorMgmtState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _obj: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_output_v1::Request;
        match request {
            Request::GetImageDescription { image_description } => {
                // Our output is SDR sRGB by default.
                let meta = HdrMetadata {
                    transfer: Some(TransferFunction::Gamma22),
                    primaries: Some(Primaries::Srgb),
                    ..Default::default()
                };
                let identity = next_identity();
                let desc = data_init.init(
                    image_description,
                    ImageDescriptionData {
                        meta,
                        allow_info: true,
                        identity,
                    },
                );
                send_ready(&desc, identity);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: ColorMgmt> Dispatch<WpImageDescriptionCreatorIccV1, (), D> for ColorMgmtState {
    fn request(
        _state: &mut D,
        _client: &Client,
        obj: &WpImageDescriptionCreatorIccV1,
        request: wp_image_description_creator_icc_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_image_description_creator_icc_v1::Request;
        match request {
            // ICC is not advertised; if a client creates a description anyway,
            // fail it gracefully.
            Request::Create { image_description } => {
                let identity = next_identity();
                let desc = data_init.init(
                    image_description,
                    ImageDescriptionData {
                        meta: HdrMetadata::default(),
                        allow_info: false,
                        identity,
                    },
                );
                desc.failed(
                    wp_image_description_v1::Cause::Unsupported,
                    "ICC image descriptions are not supported".into(),
                );
            }
            _ => {
                let _ = obj;
            }
        }
    }
}

impl<D: ColorMgmt> Dispatch<WpImageDescriptionReferenceV1, (), D> for ColorMgmtState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _obj: &WpImageDescriptionReferenceV1,
        _request: wp_image_description_reference_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
    }
}

/// Create the `wp_color_manager_v1` global (version 3).
pub fn create_color_management_global<D: ColorMgmt>(dh: &DisplayHandle) -> GlobalId {
    dh.create_global::<D, WpColorManagerV1, _>(3, ())
}

macro_rules! delegate_color_management {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpColorManagerV1: ()
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpColorManagerV1: ()
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpColorManagementOutputV1: ()
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpColorManagementSurfaceV1: $crate::wayland::protocols::color_management::CmSurfaceData
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpColorManagementSurfaceFeedbackV1: $crate::wayland::protocols::color_management::CmSurfaceData
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpImageDescriptionCreatorIccV1: ()
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpImageDescriptionCreatorParamsV1: $crate::wayland::protocols::color_management::ParamsData
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpImageDescriptionV1: $crate::wayland::protocols::color_management::ImageDescriptionData
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpImageDescriptionInfoV1: $crate::wayland::protocols::color_management::ImageDescriptionData
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::color_management::WpImageDescriptionReferenceV1: ()
        ] => $crate::wayland::protocols::color_management::ColorMgmtState);
    };
}
pub(crate) use delegate_color_management;
