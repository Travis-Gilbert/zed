#![cfg_attr(target_family = "wasm", no_main)]

use gpui::wgpu;
use gpui::wgpu::util::DeviceExt;
use gpui::{
    AnyElement, App, Bounds, Context, ExternalGpuSurfaceError, ExternalGpuSurfaceHandle,
    ExternalGpuSurfaceStatus, Render, SharedString, TransformationMatrix, Window, WindowBounds,
    WindowOptions, div, external_gpu_surface, prelude::*, px, rgb, size,
};
use gpui_platform::application;
use web_time::Instant;

const TRIANGLE_SHADER: &str = r#"
struct Uniforms {
    angle: f32,
    aspect: f32,
    pulse: f32,
    padding: f32,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec3<f32>,
}

@vertex
fn vertex_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let positions = array(
        vec2<f32>(0.0, 0.72),
        vec2<f32>(-0.72, -0.62),
        vec2<f32>(0.72, -0.62),
    );
    let colors = array(
        vec3<f32>(0.22, 0.86, 1.0),
        vec3<f32>(0.82, 0.34, 1.0),
        vec3<f32>(1.0, 0.64, 0.24),
    );
    let rotation = mat2x2<f32>(
        cos(uniforms.angle), -sin(uniforms.angle),
        sin(uniforms.angle), cos(uniforms.angle),
    );
    var position = rotation * positions[vertex_index];
    position.x = position.x / uniforms.aspect;

    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.color = colors[vertex_index] * uniforms.pulse;
    return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(input.color, 1.0);
}
"#;

struct TriangleGpu {
    surface: ExternalGpuSurfaceHandle,
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
}

impl TriangleGpu {
    fn new(surface: ExternalGpuSurfaceHandle) -> Self {
        let device = surface.device();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("external_gpu_surface_triangle_shader"),
            source: wgpu::ShaderSource::Wgsl(TRIANGLE_SHADER.into()),
        });
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("external_gpu_surface_triangle_uniform"),
            contents: &uniform_bytes([0.0, 1.0, 1.0, 0.0]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("external_gpu_surface_triangle_uniform_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("external_gpu_surface_triangle_uniform_bind_group"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("external_gpu_surface_triangle_pipeline_layout"),
            bind_group_layouts: &[Some(&uniform_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("external_gpu_surface_triangle_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface.format(),
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            surface,
            pipeline,
            uniform,
            uniform_bind_group,
        }
    }

    fn render(&self, elapsed_seconds: f32) -> Result<bool, ExternalGpuSurfaceError> {
        if self.surface.has_unconsumed_frame()? {
            return Ok(false);
        }

        let (width, height) = self.surface.size()?;
        let angle = elapsed_seconds * 0.72;
        let aspect = width as f32 / height.max(1) as f32;
        let pulse = 0.82 + 0.18 * (elapsed_seconds * 1.7).sin();
        self.surface.queue().write_buffer(
            &self.uniform,
            0,
            &uniform_bytes([angle, aspect, pulse, 0.0]),
        );

        let mut encoder =
            self.surface
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("external_gpu_surface_triangle_encoder"),
                });
        let frame = self.surface.acquire_frame()?;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("external_gpu_surface_triangle_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: frame.view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.025,
                            g: 0.035,
                            b: 0.075,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.uniform_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        frame.submit_and_present([encoder.finish()])?;
        Ok(true)
    }
}

struct ExternalSurfaceDemo {
    gpu: Option<TriangleGpu>,
    error: Option<SharedString>,
    started_at: Instant,
    paused: bool,
    translucent: bool,
    submitted_frames: u64,
    last_title_state: Option<(bool, ExternalGpuSurfaceStatus, u64)>,
}

impl ExternalSurfaceDemo {
    fn new(window: &mut Window) -> Self {
        let (gpu, error) =
            match window.create_external_gpu_surface(960, 540, wgpu::TextureFormat::Rgba8UnormSrgb)
            {
                Ok(surface) => (Some(TriangleGpu::new(surface)), None),
                Err(error) => (None, Some(error.to_string().into())),
            };
        Self {
            gpu,
            error,
            started_at: Instant::now(),
            paused: false,
            translucent: false,
            submitted_frames: 0,
            last_title_state: None,
        }
    }

    fn toggle(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.paused = !self.paused;
        self.translucent = !self.translucent;
        cx.notify();
    }
}

impl Render for ExternalSurfaceDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.paused {
            if let Some(gpu) = &self.gpu {
                match gpu.render(self.started_at.elapsed().as_secs_f32()) {
                    Ok(true) => self.submitted_frames += 1,
                    Ok(false) => {}
                    Err(error) => {
                        self.error = Some(error.to_string().into());
                    }
                }
            }
            window.request_animation_frame();
        }

        let status = self
            .gpu
            .as_ref()
            .map_or(ExternalGpuSurfaceStatus::Closed, |gpu| gpu.surface.status());
        let texture_bytes = self
            .gpu
            .as_ref()
            .and_then(|gpu| gpu.surface.allocated_texture_bytes().ok())
            .unwrap_or(0);
        let title_state = (self.paused, status, self.submitted_frames / 60);
        if self.last_title_state != Some(title_state) {
            let activity = if self.paused { "paused" } else { "live" };
            window.set_window_title(&format!(
                "GPUI external GPU surface - {activity} - {status:?} - {} frames",
                self.submitted_frames
            ));
            self.last_title_state = Some(title_state);
        }
        let surface: AnyElement = if let Some(gpu) = &self.gpu {
            external_gpu_surface(gpu.surface.clone())
                .with_transformation(TransformationMatrix {
                    rotation_scale: [[0.985, 0.0], [0.0, 0.985]],
                    translation: [4.0, 3.0],
                })
                .size_full()
                .opacity(if self.translucent { 0.62 } else { 1.0 })
                .into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(0xff8f8f))
                .child(
                    self.error
                        .clone()
                        .unwrap_or_else(|| "external GPU surface unavailable".into()),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .bg(rgb(0x090b16))
            .text_color(rgb(0xf2f5ff))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_2xl().child("GPUI external GPU surface"))
                            .child(
                                div().text_sm().text_color(rgb(0x9ba7c7)).child(
                                    "One wgpu texture, composed as an ordinary GPUI element",
                                ),
                            ),
                    )
                    .child(div().text_sm().child(format!(
                        "status: {status:?} | frames: {}",
                        self.submitted_frames
                    ))),
            )
            .child(
                div()
                    .id("external-surface-hitbox")
                    .relative()
                    .flex_1()
                    .min_h(px(320.0))
                    .rounded_xl()
                    .overflow_hidden()
                    .border_1()
                    .border_color(rgb(0x53618c))
                    .on_click(cx.listener(Self::toggle))
                    .child(div().absolute().inset_0().bg(rgb(0x231a3d)))
                    .child(surface)
                    .child(
                        div()
                            .absolute()
                            .top_4()
                            .left_4()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(0x171c31))
                            .text_sm()
                            .child(if self.paused {
                                "Paused; click to resume"
                            } else {
                                "Live; click to pause and test opacity"
                            }),
                    )
                    .child(
                        div()
                            .absolute()
                            .bottom_4()
                            .right_4()
                            .text_sm()
                            .text_color(rgb(0xbac5e8))
                            .child(format!("bounded texture bytes: {texture_bytes}")),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x9ba7c7))
                    .child("Resize the window to exercise coalesced device-pixel allocation."),
            )
    }
}

fn uniform_bytes(values: [f32; 4]) -> [u8; 16] {
    let mut bytes = [0; 16];
    for (destination, value) in bytes.chunks_exact_mut(4).zip(values) {
        destination.copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

fn launch(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(960.0), px(680.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, cx| cx.new(|_| ExternalSurfaceDemo::new(window)),
    )
    .expect("failed to open external GPU surface example window");
    cx.activate(true);
}

#[cfg(not(target_family = "wasm"))]
fn run_example() {
    application().run(launch);
}

#[cfg(target_family = "wasm")]
thread_local! {
    static APPLICATION: std::cell::RefCell<Option<gpui::ApplicationHandle>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(target_family = "wasm")]
fn run_example() {
    let application = application().run_embedded(launch);
    APPLICATION.with(|slot| slot.replace(Some(application)));
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    env_logger::init();
    run_example();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run_example();
}
