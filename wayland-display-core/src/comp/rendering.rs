use std::time::{Duration, Instant};

use super::State;
use crate::utils::allocator::GsBuffer;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::{Bind, Frame, Offscreen};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};
use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer,
        damage::{Error as OutputDamageTrackerError, RenderOutputResult},
        element::{
            Kind, memory::MemoryRenderBufferRenderElement, surface::WaylandSurfaceRenderElement,
        },
    },
    desktop::space::render_output,
    input::pointer::CursorImageStatus,
    render_elements,
};

pub const CURSOR_DATA_BYTES: &[u8] = include_bytes!("../../resources/cursor.rgba");

render_elements! {
    CursorElement<R> where R: Renderer + ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Memory=MemoryRenderBufferRenderElement<R>
}

/// Fragment shader: BT.709 SDR (sRGB-encoded) -> linear -> scale to the SDR
/// reference white (nits) -> BT.2020 primaries -> PQ (SMPTE ST 2084). Plugs into
/// Smithay's custom texture-shader template (`//_DEFINES` must stay first so the
/// renderer can inject `#extension`/`#define` lines right after `#version`).
pub(crate) const HDR_PQ_SHADER: &str = r#"//_DEFINES
precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
uniform float sdr_reference;
varying vec2 v_coords;

const mat3 bt709_to_bt2020 = mat3(
    0.627403914, 0.069097289, 0.016391439,
    0.329283038, 0.919540395, 0.088013308,
    0.043313048, 0.011362316, 0.895595253
);

vec3 pq_oetf(vec3 l) {
    float m1 = 0.1593017578125;
    float m2 = 78.84375;
    float c1 = 0.8359375;
    float c2 = 18.8515625;
    float c3 = 18.6875;
    vec3 lm = pow(clamp(l, 0.0, 1.0), vec3(m1));
    return pow((c1 + c2 * lm) / (1.0 + c3 * lm), vec3(m2));
}

void main() {
    vec4 color = texture2D(tex, v_coords);
#ifdef NO_ALPHA
    color = vec4(color.rgb, 1.0);
#endif
    vec3 linear = pow(max(color.rgb, vec3(0.0)), vec3(2.2));
    vec3 scaled = linear * (sdr_reference / 10000.0);
    vec3 rec2020 = bt709_to_bt2020 * scaled;
    vec3 pq = pq_oetf(rec2020);
    gl_FragColor = vec4(pq, color.a) * alpha;
}
"#;

/// Compile the HDR PQ texture shader. Returns None (HDR disabled, SDR used) on
/// failure so the compositor still works.
pub(crate) fn compile_pq_program(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    match renderer.compile_custom_texture_shader(
        HDR_PQ_SHADER,
        &[UniformName::new("sdr_reference", UniformType::_1f)],
    ) {
        Ok(program) => Some(program),
        Err(err) => {
            tracing::warn!(?err, "Failed to compile HDR PQ shader; HDR output disabled");
            None
        }
    }
}

impl State {
    pub fn create_frame(
        &mut self,
    ) -> Result<(gst::Buffer, RenderOutputResult), OutputDamageTrackerError<GlesError>> {
        assert!(self.output.is_some());
        assert!(self.dtr.is_some());
        assert!(self.video_info.is_some());
        assert!(self.output_buffer.is_some());

        let elements =
            if Instant::now().duration_since(self.last_pointer_movement) < Duration::from_secs(5) {
                match &self.cursor_state {
                CursorImageStatus::Named(_cursor_icon) => vec![CursorElement::Memory(
                    // TODO: icon?
                    MemoryRenderBufferRenderElement::from_buffer(
                        &mut self.renderer,
                        self.pointer_location.to_physical_precise_round(1),
                        &self.cursor_element,
                        None,
                        None,
                        None,
                        Kind::Cursor,
                    )
                    .map_err(OutputDamageTrackerError::Rendering)?,
                )],
                CursorImageStatus::Surface(wl_surface) => {
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        &mut self.renderer,
                        wl_surface,
                        self.pointer_location.to_physical_precise_round(1),
                        1.,
                        1.,
                        Kind::Cursor,
                    )
                }
                CursorImageStatus::Hidden => vec![],
            }
            } else {
                vec![]
            };

        // HDR path: composite to an intermediate, then BT.2020 + PQ post-process
        // into the (10-bit) output buffer. Gated so the SDR path is unchanged.
        if self.pq_program.is_some() && self.active_surface_is_hdr() {
            return self.create_frame_hdr(elements);
        }

        let mut output_buffer = self.output_buffer.clone().expect("Output buffer not set");

        let mut target = output_buffer
            .bind(&mut self.renderer)
            .map_err(OutputDamageTrackerError::Rendering)?;

        let render_output_result = render_output(
            self.output.as_ref().unwrap(),
            &mut self.renderer,
            &mut target,
            1.0,
            0,
            [&self.space],
            &*elements,
            self.dtr.as_mut().unwrap(),
            [0.0, 0.0, 0.0, 1.0],
        )?;

        match self
            .output_buffer
            .clone()
            .unwrap()
            .to_gs_buffer(&mut target, &mut self.renderer)
        {
            Ok(buffer) => Ok((buffer, render_output_result)),
            Err(e) => {
                tracing::warn!("Failed to convert buffer to gst buffer: {:?}", e);
                Err(OutputDamageTrackerError::Rendering(GlesError::MappingError))
            }
        }
    }

    /// True if the active (fullscreen) surface carries HDR (PQ/BT.2020) metadata.
    fn active_surface_is_hdr(&self) -> bool {
        let map = &self.surface_hdr_metadata;
        if map.is_empty() {
            return false;
        }
        self.space.elements().any(|w| {
            w.toplevel()
                .and_then(|t| map.get(t.wl_surface()))
                .map(|m| m.is_hdr())
                .unwrap_or(false)
        })
    }

    /// HDR two-pass render: composite into an 8-bit intermediate, then convert
    /// BT.709 SDR -> BT.2020 + PQ in a custom shader pass into the 10-bit output.
    ///
    /// NOTE: first-pass implementation. Assumes composited content is sRGB/BT.709
    /// SDR and tone-maps diffuse white to ~203 nits; a passthrough branch for
    /// clients that already submit BT.2020/PQ (gamescope --hdr-enabled) still
    /// needs adding, and the color math must be validated/tuned on the GPU.
    fn create_frame_hdr(
        &mut self,
        elements: Vec<CursorElement<GlesRenderer>>,
    ) -> Result<(gst::Buffer, RenderOutputResult), OutputDamageTrackerError<GlesError>> {
        let (w, h) = {
            let vi = self.video_info.as_ref().unwrap();
            (vi.width() as i32, vi.height() as i32)
        };
        let size: Size<i32, Physical> = (w, h).into();
        let buf_size: Size<i32, BufferCoord> = (w, h).into();

        let recreate = match &self.hdr_intermediate {
            Some((_, s)) => *s != size,
            None => true,
        };
        if recreate {
            let tex = self
                .renderer
                .create_buffer(Fourcc::Abgr8888, buf_size)
                .map_err(OutputDamageTrackerError::Rendering)?;
            self.hdr_intermediate = Some((tex, size));
        }

        // Pass 1: composite the scene into the intermediate (sRGB/BT.709, 8-bit).
        let render_output_result = {
            let (inter, _) = self.hdr_intermediate.as_mut().unwrap();
            let mut inter_target = self
                .renderer
                .bind(inter)
                .map_err(OutputDamageTrackerError::Rendering)?;
            render_output(
                self.output.as_ref().unwrap(),
                &mut self.renderer,
                &mut inter_target,
                1.0,
                0,
                [&self.space],
                &*elements,
                self.dtr.as_mut().unwrap(),
                [0.0, 0.0, 0.0, 1.0],
            )?
        };

        // Pass 2: BT.2020 + PQ post-process into the (10-bit) output buffer.
        let mut output_buffer = self.output_buffer.clone().expect("Output buffer not set");
        let mut out_target = output_buffer
            .bind(&mut self.renderer)
            .map_err(OutputDamageTrackerError::Rendering)?;
        {
            let dst = Rectangle::from_size(size);
            let src = Rectangle::from_size(buf_size).to_f64();
            let mut frame = self
                .renderer
                .render(&mut out_target, size, Transform::Normal)
                .map_err(OutputDamageTrackerError::Rendering)?;
            let (inter, _) = self.hdr_intermediate.as_ref().unwrap();
            frame
                .render_texture_from_to(
                    inter,
                    src,
                    dst,
                    &[dst],
                    &[dst],
                    Transform::Normal,
                    1.0,
                    self.pq_program.as_ref(),
                    &[Uniform::new("sdr_reference", 203.0f32)],
                )
                .map_err(OutputDamageTrackerError::Rendering)?;
            let _ = frame
                .finish()
                .map_err(OutputDamageTrackerError::Rendering)?
                .wait();
        }

        match self
            .output_buffer
            .clone()
            .unwrap()
            .to_gs_buffer(&mut out_target, &mut self.renderer)
        {
            Ok(buffer) => Ok((buffer, render_output_result)),
            Err(e) => {
                tracing::warn!("Failed to convert buffer to gst buffer: {:?}", e);
                Err(OutputDamageTrackerError::Rendering(GlesError::MappingError))
            }
        }
    }
}
