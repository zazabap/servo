/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::borrow::ToOwned;
use std::collections::HashMap;
use std::sync::Arc;
use std::{f32, thread};

use canvas_traits::ConstellationCanvasMsg;
use canvas_traits::canvas::*;
use compositing_traits::CrossProcessCompositorApi;
use crossbeam_channel::{Sender, select, unbounded};
use euclid::default::{Rect, Size2D, Transform2D};
use fonts::{FontContext, SystemFontServiceProxy};
use ipc_channel::ipc::{self, IpcSender};
use ipc_channel::router::ROUTER;
use log::warn;
use net_traits::ResourceThreads;
use pixels::Snapshot;
use style::color::AbsoluteColor;
use style::properties::style_structs::Font as FontStyleStruct;
use webrender_api::ImageKey;

use crate::canvas_data::*;
use crate::raqote_backend::RaqoteBackend;

pub struct CanvasPaintThread<'a> {
    canvases: HashMap<CanvasId, Canvas<'a>>,
    next_canvas_id: CanvasId,
    compositor_api: CrossProcessCompositorApi,
    font_context: Arc<FontContext>,
}

impl<'a> CanvasPaintThread<'a> {
    fn new(
        compositor_api: CrossProcessCompositorApi,
        system_font_service: Arc<SystemFontServiceProxy>,
        resource_threads: ResourceThreads,
    ) -> CanvasPaintThread<'a> {
        CanvasPaintThread {
            canvases: HashMap::new(),
            next_canvas_id: CanvasId(0),
            compositor_api: compositor_api.clone(),
            font_context: Arc::new(FontContext::new(
                system_font_service,
                compositor_api,
                resource_threads,
            )),
        }
    }

    /// Creates a new `CanvasPaintThread` and returns an `IpcSender` to
    /// communicate with it.
    pub fn start(
        compositor_api: CrossProcessCompositorApi,
        system_font_service: Arc<SystemFontServiceProxy>,
        resource_threads: ResourceThreads,
    ) -> (Sender<ConstellationCanvasMsg>, IpcSender<CanvasMsg>) {
        let (ipc_sender, ipc_receiver) = ipc::channel::<CanvasMsg>().unwrap();
        let msg_receiver = ROUTER.route_ipc_receiver_to_new_crossbeam_receiver(ipc_receiver);
        let (create_sender, create_receiver) = unbounded();
        thread::Builder::new()
            .name("Canvas".to_owned())
            .spawn(move || {
                let mut canvas_paint_thread = CanvasPaintThread::new(
                    compositor_api, system_font_service, resource_threads);
                loop {
                    select! {
                        recv(msg_receiver) -> msg => {
                            match msg {
                                Ok(CanvasMsg::Canvas2d(message, canvas_id)) => {
                                    canvas_paint_thread.process_canvas_2d_message(message, canvas_id);
                                },
                                Ok(CanvasMsg::Close(canvas_id)) => {
                                    canvas_paint_thread.canvases.remove(&canvas_id);
                                },
                                Ok(CanvasMsg::Recreate(size, canvas_id)) => {
                                    canvas_paint_thread.canvas(canvas_id).recreate(size);
                                },
                                Ok(CanvasMsg::FromScript(message, canvas_id)) => match message {
                                    FromScriptMsg::SendPixels(chan) => {
                                        chan.send(canvas_paint_thread
                                            .canvas(canvas_id)
                                            .read_pixels(None, None)
                                            .as_ipc()
                                        ).unwrap();
                                    },
                                },
                                Err(e) => {
                                    warn!("Error on CanvasPaintThread receive ({})", e);
                                },
                            }
                        }
                        recv(create_receiver) -> msg => {
                            match msg {
                                Ok(ConstellationCanvasMsg::Create { sender: creator, size }) => {
                                    let canvas_data = canvas_paint_thread.create_canvas(size);
                                    creator.send(canvas_data).unwrap();
                                },
                                Ok(ConstellationCanvasMsg::Exit(exit_sender)) => {
                                    let _ = exit_sender.send(());
                                    break;
                                },
                                Err(e) => {
                                    warn!("Error on CanvasPaintThread receive ({})", e);
                                    break;
                                },
                            }
                        }
                    }
                }
            })
            .expect("Thread spawning failed");

        (create_sender, ipc_sender)
    }

    pub fn create_canvas(&mut self, size: Size2D<u64>) -> (CanvasId, ImageKey) {
        let canvas_id = self.next_canvas_id;
        self.next_canvas_id.0 += 1;

        let canvas_data = CanvasData::new(
            size,
            self.compositor_api.clone(),
            self.font_context.clone(),
            RaqoteBackend,
        );
        let image_key = canvas_data.image_key();
        self.canvases.insert(canvas_id, Canvas::Raqote(canvas_data));

        (canvas_id, image_key)
    }

    fn process_canvas_2d_message(&mut self, message: Canvas2dMsg, canvas_id: CanvasId) {
        match message {
            Canvas2dMsg::FillText(
                text,
                x,
                y,
                max_width,
                style,
                is_rtl,
                text_options,
                shadow_options,
                composition_options,
                transform,
            ) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_transform(&transform);
                canvas.set_fill_style(style);
                canvas.set_text_options(text_options);
                canvas.set_shadow_options(shadow_options);
                canvas.set_composition_options(composition_options);
                canvas.fill_text(text, x, y, max_width, is_rtl);
            },
            Canvas2dMsg::FillRect(rect, style, shadow_options, composition_options, transform) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_transform(&transform);
                canvas.set_fill_style(style);
                canvas.set_shadow_options(shadow_options);
                canvas.set_composition_options(composition_options);
                canvas.fill_rect(&rect);
            },
            Canvas2dMsg::StrokeRect(
                rect,
                style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_transform(&transform);
                canvas.set_stroke_style(style);
                canvas.set_line_options(line_options);
                canvas.set_shadow_options(shadow_options);
                canvas.set_composition_options(composition_options);
                canvas.stroke_rect(&rect);
            },
            Canvas2dMsg::ClearRect(ref rect, transform) => {
                self.canvas(canvas_id).set_transform(&transform);
                self.canvas(canvas_id).clear_rect(rect)
            },
            Canvas2dMsg::FillPath(style, path, shadow_options, composition_options, transform) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_transform(&transform);
                canvas.set_fill_style(style);
                canvas.set_shadow_options(shadow_options);
                canvas.set_composition_options(composition_options);
                canvas.fill_path(&path);
            },
            Canvas2dMsg::StrokePath(
                path,
                style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_transform(&transform);
                canvas.set_stroke_style(style);
                canvas.set_line_options(line_options);
                canvas.set_shadow_options(shadow_options);
                canvas.set_composition_options(composition_options);
                canvas.stroke_path(&path);
            },
            Canvas2dMsg::ClipPath(path, transform) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_transform(&transform);
                canvas.clip_path(&path);
            },
            Canvas2dMsg::DrawImage(
                snapshot,
                dest_rect,
                source_rect,
                smoothing_enabled,
                shadow_options,
                composition_options,
                transform,
            ) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_transform(&transform);
                canvas.set_shadow_options(shadow_options);
                canvas.set_composition_options(composition_options);
                canvas.draw_image(
                    snapshot.to_owned(),
                    dest_rect,
                    source_rect,
                    smoothing_enabled,
                )
            },
            Canvas2dMsg::DrawEmptyImage(
                image_size,
                dest_rect,
                source_rect,
                shadow_options,
                composition_options,
                transform,
            ) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_transform(&transform);
                canvas.set_shadow_options(shadow_options);
                canvas.set_composition_options(composition_options);
                self.canvas(canvas_id).draw_image(
                    Snapshot::cleared(image_size),
                    dest_rect,
                    source_rect,
                    false,
                )
            },
            Canvas2dMsg::DrawImageInOther(
                other_canvas_id,
                image_size,
                dest_rect,
                source_rect,
                smoothing,
                shadow_options,
                composition_options,
                transform,
            ) => {
                let snapshot = self
                    .canvas(canvas_id)
                    .read_pixels(Some(source_rect.to_u32()), Some(image_size));
                let canvas = self.canvas(other_canvas_id);
                canvas.set_transform(&transform);
                canvas.set_composition_options(composition_options);
                canvas.set_shadow_options(shadow_options);
                canvas.draw_image(snapshot, dest_rect, source_rect, smoothing);
            },
            Canvas2dMsg::MeasureText(text, sender, text_options) => {
                let canvas = self.canvas(canvas_id);
                canvas.set_text_options(text_options);
                let metrics = canvas.measure_text(text);
                sender.send(metrics).unwrap();
            },
            Canvas2dMsg::RestoreContext => self.canvas(canvas_id).restore_context_state(),
            Canvas2dMsg::SaveContext => self.canvas(canvas_id).save_context_state(),
            Canvas2dMsg::GetImageData(dest_rect, canvas_size, sender) => {
                let snapshot = self
                    .canvas(canvas_id)
                    .read_pixels(Some(dest_rect), Some(canvas_size));
                sender.send(snapshot.as_ipc()).unwrap();
            },
            Canvas2dMsg::PutImageData(rect, snapshot) => {
                self.canvas(canvas_id)
                    .put_image_data(snapshot.to_owned(), rect);
            },
            Canvas2dMsg::UpdateImage(sender) => {
                self.canvas(canvas_id).update_image_rendering();
                sender.send(()).unwrap();
            },
        }
    }

    fn canvas(&mut self, canvas_id: CanvasId) -> &mut Canvas<'a> {
        self.canvases.get_mut(&canvas_id).expect("Bogus canvas id")
    }
}

enum Canvas<'a> {
    Raqote(CanvasData<'a, RaqoteBackend>),
}

impl Canvas<'_> {
    fn set_fill_style(&mut self, style: FillOrStrokeStyle) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_fill_style(style),
        }
    }

    fn fill_text(&mut self, text: String, x: f64, y: f64, max_width: Option<f64>, is_rtl: bool) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.fill_text(text, x, y, max_width, is_rtl),
        }
    }

    fn fill_rect(&mut self, rect: &Rect<f32>) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.fill_rect(rect),
        }
    }

    fn set_stroke_style(&mut self, style: FillOrStrokeStyle) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_stroke_style(style),
        }
    }

    fn stroke_rect(&mut self, rect: &Rect<f32>) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.stroke_rect(rect),
        }
    }

    fn fill_path(&mut self, path: &Path) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.fill_path(path),
        }
    }

    fn stroke_path(&mut self, path: &Path) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.stroke_path(path),
        }
    }

    fn clear_rect(&mut self, rect: &Rect<f32>) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.clear_rect(rect),
        }
    }

    fn draw_image(
        &mut self,
        snapshot: Snapshot,
        dest_rect: Rect<f64>,
        source_rect: Rect<f64>,
        smoothing_enabled: bool,
    ) {
        match self {
            Canvas::Raqote(canvas_data) => {
                canvas_data.draw_image(snapshot, dest_rect, source_rect, smoothing_enabled)
            },
        }
    }

    fn read_pixels(
        &mut self,
        read_rect: Option<Rect<u32>>,
        canvas_size: Option<Size2D<u32>>,
    ) -> Snapshot {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.read_pixels(read_rect, canvas_size),
        }
    }

    fn restore_context_state(&mut self) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.restore_context_state(),
        }
    }

    fn save_context_state(&mut self) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.save_context_state(),
        }
    }

    fn set_line_width(&mut self, width: f32) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_line_width(width),
        }
    }

    fn set_line_cap(&mut self, cap: LineCapStyle) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_line_cap(cap),
        }
    }

    fn set_line_join(&mut self, join: LineJoinStyle) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_line_join(join),
        }
    }

    fn set_miter_limit(&mut self, limit: f32) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_miter_limit(limit),
        }
    }

    fn set_line_dash(&mut self, items: Vec<f32>) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_line_dash(items),
        }
    }

    fn set_line_dash_offset(&mut self, offset: f32) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_line_dash_offset(offset),
        }
    }

    fn set_transform(&mut self, matrix: &Transform2D<f32>) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_transform(matrix),
        }
    }

    fn set_global_alpha(&mut self, alpha: f32) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_global_alpha(alpha),
        }
    }

    fn set_global_composition(&mut self, op: CompositionOrBlending) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_global_composition(op),
        }
    }

    fn set_shadow_offset_x(&mut self, value: f64) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_shadow_offset_x(value),
        }
    }

    fn set_shadow_offset_y(&mut self, value: f64) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_shadow_offset_y(value),
        }
    }

    fn set_shadow_blur(&mut self, value: f64) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_shadow_blur(value),
        }
    }

    fn set_shadow_color(&mut self, color: AbsoluteColor) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_shadow_color(color),
        }
    }

    fn set_font(&mut self, font_style: FontStyleStruct) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_font(font_style),
        }
    }

    fn set_text_align(&mut self, text_align: TextAlign) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_text_align(text_align),
        }
    }

    fn set_text_baseline(&mut self, text_baseline: TextBaseline) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.set_text_baseline(text_baseline),
        }
    }

    fn measure_text(&mut self, text: String) -> TextMetrics {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.measure_text(text),
        }
    }

    fn clip_path(&mut self, path: &Path) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.clip_path(path),
        }
    }

    fn put_image_data(&mut self, snapshot: Snapshot, rect: Rect<u32>) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.put_image_data(snapshot, rect),
        }
    }

    fn update_image_rendering(&mut self) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.update_image_rendering(),
        }
    }

    fn recreate(&mut self, size: Option<Size2D<u64>>) {
        match self {
            Canvas::Raqote(canvas_data) => canvas_data.recreate(size),
        }
    }

    fn set_text_options(&mut self, text_options: TextOptions) {
        if let Some(font) = text_options.font {
            self.set_font(font);
        }
        self.set_text_align(text_options.align);
        self.set_text_baseline(text_options.baseline);
    }

    fn set_shadow_options(&mut self, shadow_options: ShadowOptions) {
        self.set_shadow_color(shadow_options.color);
        self.set_shadow_offset_x(shadow_options.offset_x);
        self.set_shadow_offset_y(shadow_options.offset_y);
        self.set_shadow_blur(shadow_options.blur);
    }

    fn set_composition_options(&mut self, composition_options: CompositionOptions) {
        self.set_global_alpha(composition_options.alpha as f32);
        self.set_global_composition(composition_options.composition_operation);
    }

    fn set_line_options(&mut self, line_options: LineOptions) {
        let LineOptions {
            width,
            cap_style,
            join_style,
            miter_limit,
            dash,
            dash_offset,
        } = line_options;
        self.set_line_width(width as f32);
        self.set_line_cap(cap_style);
        self.set_line_join(join_style);
        self.set_miter_limit(miter_limit as f32);
        self.set_line_dash(dash);
        self.set_line_dash_offset(dash_offset as f32);
    }
}
