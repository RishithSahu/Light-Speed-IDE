//! Solid-rectangle renderer.
//!
//! Everything in the shell that is not text is a rectangle: panel backgrounds,
//! the caret, selection highlights, tab plates, the scrollbar. They are drawn in
//! one instanced draw call, which keeps the non-text part of a frame at a fixed,
//! trivial cost no matter how busy the window is.

use crate::layout::Rect;
use crate::theme::Color;
use std::borrow::Cow;

/// What a quad's rectangle actually paints.
///
/// Both are the same instanced rectangle on the GPU; the difference is one
/// number handed to the fragment shader, which discards the corners for an
/// ellipse. Adding a second pipeline for circles would have meant a second
/// buffer, a second draw call and a second sort order for a shape that is
/// still, geometrically, a rectangle with some pixels thrown away.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Rectangle,
    /// The ellipse inscribed in the rectangle -- a circle when it is square.
    /// Used by the dependency graph's nodes (`app::depgraph`).
    Ellipse,
}

/// One rectangle to fill.
#[derive(Copy, Clone, Debug)]
pub struct Quad {
    pub rect: Rect,
    pub color: Color,
    pub shape: Shape,
    /// Rotation about the rectangle's own centre, in radians. Zero for all
    /// the window's chrome, which is axis-aligned by construction; non-zero
    /// only for graph edges, which are thin rectangles pointed at wherever
    /// the next node happens to be.
    pub rotation: f32,
}

impl Quad {
    pub fn new(rect: Rect, color: Color) -> Self {
        Quad { rect, color, shape: Shape::Rectangle, rotation: 0.0 }
    }

    /// The ellipse inscribed in `rect`.
    pub fn ellipse(rect: Rect, color: Color) -> Self {
        Quad { rect, color, shape: Shape::Ellipse, rotation: 0.0 }
    }

    /// A line of `thickness` pixels from `start` to `end`.
    ///
    /// Still one instanced rectangle: as long as it can be rotated, a line
    /// is just a long thin one. The alternative -- stepping a series of
    /// axis-aligned squares along the diagonal -- would have meant hundreds
    /// of instances per edge to draw something the GPU can do with one.
    pub fn line(start: (f32, f32), end: (f32, f32), thickness: f32, color: Color) -> Self {
        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let length = (dx * dx + dy * dy).sqrt();
        let centre = ((start.0 + end.0) / 2.0, (start.1 + end.1) / 2.0);
        Quad {
            rect: Rect::new(
                centre.0 - length / 2.0,
                centre.1 - thickness / 2.0,
                length,
                thickness,
            ),
            color,
            shape: Shape::Rectangle,
            rotation: dy.atan2(dx),
        }
    }
}

/// Whether a rectangle would put anything on screen.
fn is_visible(quad: &Quad) -> bool {
    quad.rect.width > 0.0 && quad.rect.height > 0.0 && quad.color.srgb[3] != 0
}

/// Bytes per instance: `vec4` rect + `vec4` color + `vec4` shape params.
///
/// The shape word is a full `vec4` rather than a lone `f32` because a vertex
/// buffer's stride has to keep every attribute at its natural alignment, and
/// three floats of padding cost nothing next to the draw call they avoid.
const INSTANCE_SIZE: u64 = 48;

const SHADER: &str = r#"
struct Uniforms {
    screen: vec2<f32>,
    padding: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    // Where this pixel sits inside its own rectangle, 0..1 on both axes.
    // Only the ellipse path reads it.
    @location(1) local: vec2<f32>,
    // 0 = rectangle, 1 = ellipse. Flat: it is per-instance, not something to
    // interpolate across the triangle.
    @location(2) @interpolate(flat) shape: f32,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) shape: vec4<f32>,
) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    let corner = corners[vertex_index];
    var pixel = rect.xy + corner * rect.zw;
    // shape.y is the rotation about this rectangle's own centre. Every piece
    // of window chrome passes zero; only graph edges do not.
    let angle = shape.y;
    if (angle != 0.0) {
        let centre = rect.xy + rect.zw * 0.5;
        let offset = pixel - centre;
        let cosine = cos(angle);
        let sine = sin(angle);
        pixel = centre + vec2<f32>(
            offset.x * cosine - offset.y * sine,
            offset.x * sine + offset.y * cosine,
        );
    }
    let ndc = vec2<f32>(
        pixel.x / uniforms.screen.x * 2.0 - 1.0,
        1.0 - pixel.y / uniforms.screen.y * 2.0,
    );

    var out: VertexOutput;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.local = corner;
    out.shape = shape.x;
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    if (input.shape > 0.5) {
        // Distance from the rectangle's centre, in units where the inscribed
        // ellipse's edge is exactly 1. Feathered over roughly a pixel's worth
        // of that distance instead of a hard cutoff, so node circles do not
        // come out with staircase edges -- there is no multisampling on this
        // pipeline to smooth them afterwards.
        let offset = (input.local - vec2<f32>(0.5, 0.5)) * 2.0;
        let distance = length(offset);
        let feather = fwidth(distance);
        let alpha = 1.0 - smoothstep(1.0 - feather, 1.0, distance);
        if (alpha <= 0.0) {
            discard;
        }
        return vec4<f32>(input.color.rgb, input.color.a * alpha);
    }
    return input.color;
}
"#;

pub struct QuadRenderer {
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    /// Instances belonging to the base layer; the rest are the overlay.
    base_count: u32,
    capacity: u64,
    count: u32,
    staging: Vec<u8>,
}

impl QuadRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightspeed.quads.shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightspeed.quads.bind_group_layout"),
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

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightspeed.quads.uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lightspeed.quads.bind_group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lightspeed.quads.pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lightspeed.quads.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: INSTANCE_SIZE,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 16,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 32,
                            shader_location: 2,
                        },
                    ],
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let capacity = 256;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightspeed.quads.instances"),
            size: capacity * INSTANCE_SIZE,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        QuadRenderer {
            pipeline,
            uniforms,
            bind_group,
            instances,
            base_count: 0,
            capacity,
            count: 0,
            staging: Vec::with_capacity(capacity as usize * INSTANCE_SIZE as usize),
        }
    }

    /// Uploads this frame's rectangles, base layer first.
    ///
    /// Both layers share one instance buffer and one upload; they are drawn as
    /// two ranges of the same draw call's instances, so compositing the menu
    /// above the editor costs an extra `draw`, not an extra buffer.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_width: f32,
        screen_height: f32,
        base: &[Quad],
        overlay: &[Quad],
    ) {
        let uniform_data = [screen_width, screen_height, 0.0, 0.0];
        queue.write_buffer(&self.uniforms, 0, bytes_of_f32(&uniform_data));

        self.staging.clear();
        for quad in base.iter().chain(overlay.iter()) {
            // Degenerate rectangles would be invisible anyway; skipping them
            // keeps the instance count honest.
            if !is_visible(quad) {
                continue;
            }
            let shape = match quad.shape {
                Shape::Rectangle => 0.0,
                Shape::Ellipse => 1.0,
            };
            let values = [
                quad.rect.x,
                quad.rect.y,
                quad.rect.width,
                quad.rect.height,
                quad.color.linear[0],
                quad.color.linear[1],
                quad.color.linear[2],
                quad.color.linear[3],
                shape,
                quad.rotation,
                0.0,
                0.0,
            ];
            self.staging.extend_from_slice(bytes_of_f32(&values));
        }

        self.count = (self.staging.len() as u64 / INSTANCE_SIZE) as u32;
        // Degenerate rectangles are skipped above, so the split is counted
        // rather than assumed from the input lengths.
        self.base_count = base.iter().filter(|quad| is_visible(quad)).count() as u32;
        if self.count == 0 {
            return;
        }

        let needed = self.staging.len() as u64;
        if needed > self.capacity * INSTANCE_SIZE {
            self.capacity = (self.count as u64).next_power_of_two();
            self.instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("lightspeed.quads.instances"),
                size: self.capacity * INSTANCE_SIZE,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.instances, 0, &self.staging);
    }

    /// Draws the editor and its chrome.
    pub fn render_base(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.render_range(pass, 0, self.base_count);
    }

    /// Draws the surfaces that float above the editor. Called after the base
    /// layer's text, so an overlay surface hides the glyphs beneath it.
    pub fn render_overlay(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.render_range(pass, self.base_count, self.count);
    }

    fn render_range(&self, pass: &mut wgpu::RenderPass<'_>, first: u32, last: u32) {
        if last <= first {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..6, first..last);
    }

    /// Rectangles uploaded for the last frame, for diagnostics.
    pub fn instance_count(&self) -> u32 {
        self.count
    }
}

/// Reinterprets a float array as the little-endian bytes the GPU expects.
fn bytes_of_f32(values: &[f32]) -> &[u8] {
    // SAFETY: `f32` has no padding or invalid bit patterns, and the resulting
    // slice borrows the same memory for the same lifetime. wgpu requires
    // little-endian data, which matches every platform this targets.
    unsafe {
        std::slice::from_raw_parts(values.as_ptr() as *const u8, std::mem::size_of_val(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_bytes_are_little_endian() {
        let values = [1.0f32, -2.5];
        let bytes = bytes_of_f32(&values);
        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[0..4], &1.0f32.to_le_bytes());
        assert_eq!(&bytes[4..8], &(-2.5f32).to_le_bytes());
    }

    #[test]
    fn instance_size_matches_the_shader_layout() {
        // Four floats of rect, four of color, four of shape parameters.
        assert_eq!(INSTANCE_SIZE, 12 * std::mem::size_of::<f32>() as u64);
    }

    #[test]
    fn a_line_spans_its_endpoints_and_points_at_them() {
        // A horizontal line: length is the span, and no rotation is needed.
        let flat = Quad::line((10.0, 50.0), (110.0, 50.0), 2.0, Color::rgb(255, 0, 0));
        assert!((flat.rect.width - 100.0).abs() < 0.01, "width is the distance covered");
        assert!((flat.rect.height - 2.0).abs() < 0.01, "height is the thickness");
        assert!(flat.rotation.abs() < 0.001, "a horizontal line needs no rotation");

        // A 45-degree line: the rectangle is still `length` long, and the
        // rotation is what puts its ends on the two points.
        let diagonal = Quad::line((0.0, 0.0), (100.0, 100.0), 2.0, Color::rgb(255, 0, 0));
        assert!((diagonal.rect.width - 141.42).abs() < 0.1, "the diagonal's true length");
        assert!(
            (diagonal.rotation - std::f32::consts::FRAC_PI_4).abs() < 0.001,
            "45 degrees, in radians"
        );
        // Centred on the midpoint, which is what the shader rotates about.
        let centre_x = diagonal.rect.x + diagonal.rect.width / 2.0;
        let centre_y = diagonal.rect.y + diagonal.rect.height / 2.0;
        assert!((centre_x - 50.0).abs() < 0.01 && (centre_y - 50.0).abs() < 0.01);
    }

    #[test]
    fn a_zero_length_line_is_dropped_rather_than_drawn_as_a_dot() {
        let degenerate = Quad::line((10.0, 10.0), (10.0, 10.0), 2.0, Color::rgb(255, 0, 0));
        assert!(!is_visible(&degenerate), "nothing to draw between a point and itself");
    }

    #[test]
    fn a_rectangle_and_an_ellipse_differ_only_in_their_shape_word() {
        // Both go through one pipeline and one buffer; if these ever stopped
        // sharing a geometry the "one draw call for everything that is not
        // text" property in this module's own docs would quietly be false.
        let rect = Rect::new(10.0, 20.0, 30.0, 40.0);
        let color = Color::rgb(1, 2, 3);
        let rectangle = Quad::new(rect, color);
        let ellipse = Quad::ellipse(rect, color);
        assert_eq!(rectangle.shape, Shape::Rectangle);
        assert_eq!(ellipse.shape, Shape::Ellipse);
        assert_eq!(rectangle.rect.width, ellipse.rect.width);
        assert_eq!(rectangle.rect.height, ellipse.rect.height);
    }
}
