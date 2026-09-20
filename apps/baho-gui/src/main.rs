use std::sync::Arc;
use std::time::{Duration, Instant};

use akar_components::{
    AKAR_THEME_DARK, DataGridSortDirection, DataGridState, DataGridStyle, data_grid_begin,
    data_grid_body_begin, data_grid_body_end, data_grid_cell, data_grid_end,
    data_grid_handle_keyboard, data_grid_header_begin, data_grid_header_cell, data_grid_header_end,
};
use akar_core::AkarCore;
use akar_layout::{Dimension, Layout, NodeId, PageConfig, Size, Style};
use akar_winit::process_window_event;
use anyhow::{Context, Result};
use baho_core::open_table;
use baho_model::{Diagnostic, Severity};
use clap::Parser;
use wgpu::{
    CompositeAlphaMode, CurrentSurfaceTexture, InstanceDescriptor, PresentMode, TextureUsages,
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowAttributes},
};

use baho_gui::GridAdapter;
mod script;
use script::{ScriptRunner, parse_script};

#[derive(Debug, Parser)]
#[command(name = "baho-gui", about = "View a selected baho table")]
struct Args {
    input: std::path::PathBuf,
    #[arg(long)]
    screenshot: Option<std::path::PathBuf>,
    #[arg(long, default_value_t = 1.0)]
    delay: f64,
    #[arg(long)]
    exit: bool,
    #[arg(long)]
    dump_layout: bool,
    #[arg(long)]
    dump_frame: Option<std::path::PathBuf>,
    #[arg(long)]
    script: Option<std::path::PathBuf>,
}

struct AppState {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    core: AkarCore,
    layout: Layout,
    page: akar_layout::PageLayout,
    grid_node: NodeId,
    grid_state: DataGridState,
    adapter: GridAdapter,
    row_keys: Vec<u64>,
    descriptors: Vec<akar_components::DataGridColumn>,
}

struct App {
    state: Option<AppState>,
    args: Args,
    initial_adapter: Option<GridAdapter>,
    fatal_error: Option<anyhow::Error>,
    script: Option<ScriptRunner>,
    start_time: Option<Instant>,
    screenshot_taken: bool,
    layout_dumped: bool,
    frame_dumped: bool,
}

fn write_png(path: &std::path::Path, frame: akar_core::CapturedFrame) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("could not create screenshot {}", path.display()))?;
    let mut encoder = png::Encoder::new(file, frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(&frame.rgba))
        .with_context(|| format!("could not write screenshot {}", path.display()))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("baho-gui: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    if args.delay < 0.0 || !args.delay.is_finite() {
        anyhow::bail!("--delay must be a finite non-negative number");
    }
    if args.script.is_some() && args.screenshot.is_some() {
        anyhow::bail!(
            "--script supplies its own screenshot steps; do not combine it with --screenshot"
        );
    }
    let script = args
        .script
        .as_ref()
        .map(|path| {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("could not read script {}", path.display()))?;
            let steps = parse_script(&contents).map_err(|error| {
                anyhow::anyhow!("could not parse script {}: {error}", path.display())
            })?;
            Ok::<_, anyhow::Error>(ScriptRunner::new(steps))
        })
        .transpose()?;
    let table = open_table(&args.input).map_err(|failure| anyhow::anyhow!(failure.to_string()))?;
    if let Some(summary) = format_startup_diagnostics(&table.diagnostics) {
        eprintln!("{summary}");
    }
    let adapter =
        GridAdapter::from_opened_table(&table).context("could not adapt opened table to a grid")?;
    let event_loop = EventLoop::new().context("could not create event loop")?;
    let mut app = App {
        state: None,
        args,
        initial_adapter: Some(adapter),
        fatal_error: None,
        script,
        start_time: None,
        screenshot_taken: false,
        layout_dumped: false,
        frame_dumped: false,
    };
    let event_loop_result = event_loop.run_app(&mut app);
    finish_event_loop(event_loop_result, app.fatal_error)
}

const MAX_DIAGNOSTIC_GROUPS: usize = 8;

fn format_startup_diagnostics(diagnostics: &[Diagnostic]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }

    let mut severity_counts = [0_usize; 3];
    let mut groups = std::collections::BTreeMap::new();
    for diagnostic in diagnostics {
        let (rank, label) = match diagnostic.severity {
            Severity::Error => (0_u8, "error"),
            Severity::Warning => (1, "warning"),
            Severity::Info => (2, "info"),
        };
        severity_counts[usize::from(rank)] += 1;
        *groups
            .entry((
                rank,
                label,
                diagnostic.stage.as_str(),
                diagnostic.code.as_str(),
            ))
            .or_insert(0_usize) += 1;
    }

    let mut summary = format!(
        "baho-gui: {} diagnostic(s): {} error, {} warning, {} info",
        diagnostics.len(),
        severity_counts[0],
        severity_counts[1],
        severity_counts[2]
    );
    for ((_, severity, stage, code), count) in groups.iter().take(MAX_DIAGNOSTIC_GROUPS) {
        use std::fmt::Write as _;
        let _ = write!(
            summary,
            "\nbaho-gui: {severity} [{stage}] {code} ({count} occurrence(s))"
        );
    }
    if groups.len() > MAX_DIAGNOSTIC_GROUPS {
        use std::fmt::Write as _;
        let _ = write!(
            summary,
            "\nbaho-gui: {} additional diagnostic group(s) omitted",
            groups.len() - MAX_DIAGNOSTIC_GROUPS
        );
    }
    Some(summary)
}

fn finish_event_loop(
    event_loop_result: Result<(), winit::error::EventLoopError>,
    fatal_error: Option<anyhow::Error>,
) -> Result<()> {
    if let Some(error) = fatal_error {
        return Err(error);
    }
    event_loop_result.context("window event loop failed")
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let title = format!(
            "baho — {}",
            self.args
                .input
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("table")
        );
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title(title)
                .with_inner_size(LogicalSize::new(1280.0, 800.0)),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.fail(
                    event_loop,
                    anyhow::anyhow!("could not create window: {error}"),
                );
                return;
            }
        };
        let instance = wgpu::Instance::new(InstanceDescriptor::new_with_display_handle(Box::new(
            event_loop.owned_display_handle(),
        )));
        let surface = match instance.create_surface(window.clone()) {
            Ok(surface) => surface,
            Err(error) => {
                self.fail(
                    event_loop,
                    anyhow::anyhow!("could not create surface: {error}"),
                );
                return;
            }
        };
        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })) {
                Ok(adapter) => adapter,
                Err(error) => {
                    self.fail(
                        event_loop,
                        anyhow::anyhow!("no compatible GPU adapter: {error}"),
                    );
                    return;
                }
            };
        let (device, queue) =
            match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())) {
                Ok(pair) => pair,
                Err(error) => {
                    self.fail(
                        event_loop,
                        anyhow::anyhow!("could not create GPU device: {error}"),
                    );
                    return;
                }
            };
        let size = window.inner_size();
        let mut surface_config = match surface.get_default_config(&adapter, size.width, size.height)
        {
            Some(config) => config,
            None => {
                self.fail(
                    event_loop,
                    anyhow::anyhow!("surface has no supported configuration"),
                );
                return;
            }
        };
        surface_config.usage = TextureUsages::RENDER_ATTACHMENT;
        surface_config.present_mode = PresentMode::Fifo;
        surface_config.alpha_mode = CompositeAlphaMode::Opaque;
        let surface_format = surface_config.format;
        surface.configure(&device, &surface_config);
        let core = AkarCore::new(
            &device,
            &queue,
            surface_format,
            akar_core::TextPipelineConfig::default(),
        );
        let mut layout = Layout::new();
        let page = layout.page(PageConfig {
            header_height: None,
            footer_height: None,
            sidebar_left_width: None,
            sidebar_right_width: None,
        });
        let grid_node = layout.new_leaf(Style {
            size: Size {
                width: Dimension::percent(1.0),
                height: Dimension::percent(1.0),
            },
            ..Default::default()
        });
        layout.register_label("grid", grid_node);
        layout.set_children(page.main, &[grid_node]);
        let Some(adapter) = self.initial_adapter.take() else {
            self.fail(event_loop, anyhow::anyhow!("table was already initialized"));
            return;
        };
        let row_keys = adapter.rows.iter().map(|row| row.key).collect();
        let descriptors = adapter
            .columns
            .iter()
            .map(|column| column.descriptor)
            .collect();
        self.start_time = Some(Instant::now());
        self.state = Some(AppState {
            window,
            device,
            queue,
            surface,
            surface_config,
            core,
            layout,
            page,
            grid_node,
            grid_state: DataGridState::new(),
            adapter,
            row_keys,
            descriptors,
        });
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if matches!(event, WindowEvent::RedrawRequested) {
            self.redraw(event_loop);
            return;
        }
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match &event {
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                state.surface_config.width = size.width;
                state.surface_config.height = size.height;
                state
                    .surface
                    .configure(&state.device, &state.surface_config);
            }
            WindowEvent::CloseRequested => event_loop.exit(),
            _ => {}
        }
        process_window_event(&mut state.core.input, &event);
        state.window.request_redraw();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let screenshot_deadline = (!self.screenshot_taken)
            .then(|| self.args.screenshot.as_ref())
            .flatten()
            .and_then(|_| self.start_time)
            .map(|start| start + Duration::from_secs_f64(self.args.delay));
        let script_deadline = self.script.as_ref().and_then(ScriptRunner::next_deadline);
        let deadline = [screenshot_deadline, script_deadline]
            .into_iter()
            .flatten()
            .min();

        match deadline {
            Some(deadline) if deadline <= now => {
                event_loop.set_control_flow(ControlFlow::Wait);
                if let Some(state) = &self.state {
                    state.window.request_redraw();
                }
            }
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

impl App {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        self.fatal_error = Some(error);
        event_loop.exit();
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let size = state.window.inner_size();
        let scale = state.window.scale_factor() as f32;
        let output = match state.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(texture)
            | CurrentSurfaceTexture::Suboptimal(texture) => texture,
            _ => return,
        };
        state.core.begin_frame(size.width, size.height, scale);
        if self.args.dump_frame.is_some() && !self.frame_dumped {
            state.core.draw_list.start_recording();
        }
        let viewport = [
            0.0,
            0.0,
            size.width as f32 / scale,
            size.height as f32 / scale,
        ];
        state.layout.compute(
            state.page.root,
            (Some(viewport[2]), Some(viewport[3])),
            |_, _, _, _, _| Size::ZERO,
        );
        if self.args.dump_layout && !self.layout_dumped {
            self.layout_dumped = true;
            for (name, rect) in state.layout.labeled_rects() {
                println!("{name} {} {} {} {}", rect[0], rect[1], rect[2], rect[3]);
            }
            event_loop.exit();
            return;
        }
        let script_capture = self.script.as_mut().and_then(|runner| {
            runner.advance(&mut state.core.input, &state.layout, Instant::now())
        });
        let style = DataGridStyle::from_theme(&AKAR_THEME_DARK);
        let row_keys = &state.row_keys;
        let descriptors = &state.descriptors;
        // Akar consumes navigation input before begin so the same frame uses
        // the updated active cell and scroll position.
        let _keyboard = data_grid_handle_keyboard(
            &mut state.core,
            &state.layout,
            state.grid_node,
            &mut state.grid_state,
            state.adapter.rows.len(),
            row_keys,
            descriptors,
            &style,
        );
        let selected = state
            .grid_state
            .has_active_cell
            .then_some(state.grid_state.active_row_key)
            .into_iter()
            .collect::<Vec<_>>();
        let response = data_grid_begin(
            &mut state.core,
            &state.layout,
            state.grid_node,
            &mut state.grid_state,
            state.adapter.rows.len(),
            row_keys,
            style.row_height,
            style.header_height,
            descriptors,
            &style,
        );
        data_grid_header_begin(&mut state.core, &response, &style);
        for column_index in response.visible_columns.clone() {
            if let Some(column) = state.adapter.columns.get(column_index) {
                let _ = data_grid_header_cell(
                    &mut state.core,
                    &state.layout,
                    &response,
                    state.grid_node,
                    column_index,
                    descriptors,
                    &style,
                    &column.display_name,
                    DataGridSortDirection::None,
                );
            }
        }
        data_grid_header_end(&mut state.core);
        data_grid_body_begin(&mut state.core, &response, row_keys, &style, &selected);
        for row_index in response.visible_rows.clone() {
            for column_index in response.visible_columns.clone() {
                let Some(row) = state.adapter.rows.get(row_index) else {
                    continue;
                };
                let Some(column) = state.adapter.columns.get(column_index) else {
                    continue;
                };
                let text = state
                    .adapter
                    .cell_text(row_index, column.source_ordinal)
                    .unwrap_or("");
                let cell = data_grid_cell(
                    &mut state.core,
                    &state.layout,
                    &response,
                    state.grid_node,
                    row_index,
                    row.key,
                    column_index,
                    descriptors,
                    &style,
                    text,
                    selected.contains(&row.key),
                );
                if cell.clicked {
                    state.grid_state.active_row_key = row.key;
                    state.grid_state.active_column_key = column.descriptor.key;
                    state.grid_state.has_active_cell = true;
                }
            }
        }
        data_grid_body_end(&mut state.core);
        data_grid_end(&mut state.core);
        let timed_capture = !self.screenshot_taken
            && self.args.screenshot.is_some()
            && self
                .start_time
                .is_some_and(|start| start.elapsed() >= Duration::from_secs_f64(self.args.delay));
        let capture = timed_capture || script_capture.is_some();
        if capture {
            state.core.request_screenshot();
        }
        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let surface_view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let render_view = if capture {
            state
                .core
                .capture_target_view(&state.device, size.width, size.height)
                .unwrap_or_else(|| {
                    output
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default())
                })
        } else {
            output
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("baho grid"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &render_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let _ = state.core.end_frame(&state.device, &state.queue, &mut pass);
        }
        if let Some(path) = self.args.dump_frame.as_ref().filter(|_| !self.frame_dumped) {
            let dump = serde_json::json!({ "recorded_calls": state.core.draw_list.recorded_calls(), "labeled_rects": state.layout.labeled_rects(), "visible_rows": response.visible_rows, "visible_columns": response.visible_columns });
            if let Err(error) = std::fs::File::create(path).and_then(|file| {
                serde_json::to_writer_pretty(file, &dump).map_err(std::io::Error::other)
            }) {
                eprintln!(
                    "baho-gui: could not write frame dump {}: {error}",
                    path.display()
                );
            }
            self.frame_dumped = true;
            state.core.draw_list.stop_recording();
        }
        if capture {
            let path = script_capture
                .or_else(|| {
                    self.args
                        .screenshot
                        .as_ref()
                        .map(|path| path.display().to_string())
                })
                .unwrap();
            match state
                .core
                .take_screenshot(&state.device, &state.queue, encoder, &output)
            {
                Ok(frame) => {
                    if let Err(error) = write_png(std::path::Path::new(&path), frame) {
                        eprintln!("baho-gui: {error:#}");
                    }
                }
                Err(error) => eprintln!("baho-gui: screenshot failed: {error}"),
            }
            self.screenshot_taken = true;
        } else {
            state.queue.submit(std::iter::once(encoder.finish()));
        }
        output.present();
        let exit_after_frame = self.args.exit
            && (capture || (self.args.screenshot.is_none() && self.script.is_none()));
        if exit_after_frame {
            event_loop.exit();
            return;
        }
        if self
            .script
            .as_ref()
            .is_some_and(|runner| !runner.is_exhausted() && runner.next_deadline().is_none())
        {
            state.window.request_redraw();
        }
        let _ = surface_view;
    }
}

#[cfg(test)]
mod tests {
    use baho_model::{Diagnostic, Severity};

    use super::{finish_event_loop, format_startup_diagnostics};

    #[test]
    fn fatal_application_error_is_returned_after_event_loop_exits_normally() {
        let error = finish_event_loop(Ok(()), Some(anyhow::anyhow!("GPU setup failed")))
            .expect_err("fatal application errors must fail the process");

        assert_eq!(error.to_string(), "GPU setup failed");
    }

    #[test]
    fn startup_diagnostics_are_grouped_deterministically_without_messages() {
        let diagnostics = vec![
            Diagnostic {
                code: "csv.ragged_record".to_string(),
                severity: Severity::Warning,
                stage: "ingest-csv".to_string(),
                message: "private cell contents: do not print".to_string(),
                location: None,
            },
            Diagnostic {
                code: "csv.detected_dialect".to_string(),
                severity: Severity::Info,
                stage: "inspection".to_string(),
                message: "comma-delimited".to_string(),
                location: None,
            },
            Diagnostic {
                code: "csv.ragged_record".to_string(),
                severity: Severity::Warning,
                stage: "ingest-csv".to_string(),
                message: "different private contents".to_string(),
                location: None,
            },
        ];

        let summary = format_startup_diagnostics(&diagnostics).expect("non-empty summary");

        assert_eq!(
            summary,
            "baho-gui: 3 diagnostic(s): 0 error, 2 warning, 1 info\n\
             baho-gui: warning [ingest-csv] csv.ragged_record (2 occurrence(s))\n\
             baho-gui: info [inspection] csv.detected_dialect (1 occurrence(s))"
        );
        assert!(!summary.contains("private"));
        assert!(!summary.contains("comma-delimited"));
    }

    #[test]
    fn startup_diagnostics_are_silent_when_none_exist() {
        assert_eq!(format_startup_diagnostics(&[]), None);
    }
}
