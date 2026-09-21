use std::sync::Arc;
use std::time::{Duration, Instant};

use akar_components::{
    AKAR_THEME_DARK, ButtonVariant, DataGridSortDirection, DataGridState, DataGridStyle,
    akar_button, akar_paragraph, akar_text_input, data_grid_begin, data_grid_body_begin,
    data_grid_body_end, data_grid_cell, data_grid_end, data_grid_handle_keyboard,
    data_grid_header_begin, data_grid_header_cell, data_grid_header_end,
};
use akar_core::AkarCore;
use akar_layout::{
    Dimension, Display, FlexDirection, Layout, NodeId, PageConfig, PageLayout, Size, Style, length,
};
use akar_winit::process_window_event;
use anyhow::{Context, Result};
use baho_core::open_table;
use baho_model::{Diagnostic, Severity};
use baho_run::{InputIdentity, Invocation};
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

use baho_gui::{GuiSession, SelectionState};
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
    page: PageLayout,
    prompt_node: NodeId,
    submit_node: NodeId,
    status_node: NodeId,
    grid_node: NodeId,
    grid_state: DataGridState,
    selection: SelectionState,
    session: GuiSession,
    runs_directory: std::path::PathBuf,
    invocation: Invocation,
}

const SIDEBAR_WIDTH: f32 = 280.0;

struct AppLayout {
    page: PageLayout,
    prompt: NodeId,
    submit: NodeId,
    status: NodeId,
    grid: NodeId,
}

fn build_app_layout(layout: &mut Layout) -> AppLayout {
    let page = layout.page(PageConfig {
        header_height: None,
        footer_height: None,
        sidebar_left_width: Some(SIDEBAR_WIDTH),
        sidebar_right_width: None,
    });
    let sidebar = page.sidebar_left.expect("left sidebar was requested");
    layout.set_style(
        sidebar,
        Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Column,
            flex_shrink: 0.0,
            gap: Size {
                width: length(0.0_f32),
                height: length(12.0_f32),
            },
            size: Size {
                width: length(SIDEBAR_WIDTH),
                height: Dimension::percent(1.0),
            },
            ..Default::default()
        },
    );
    layout.set_padding(sidebar, 16.0, 16.0, 16.0, 16.0);
    let prompt = layout.new_leaf(Style {
        flex_shrink: 0.0,
        size: Size {
            width: Dimension::percent(1.0),
            height: length(40.0_f32),
        },
        ..Default::default()
    });
    let submit = layout.new_leaf(Style {
        flex_shrink: 0.0,
        size: Size {
            width: Dimension::percent(1.0),
            height: length(40.0_f32),
        },
        ..Default::default()
    });
    let status = layout.new_leaf(Style {
        flex_shrink: 0.0,
        size: Size {
            width: Dimension::percent(1.0),
            height: length(96.0_f32),
        },
        ..Default::default()
    });
    layout.set_children(sidebar, &[prompt, submit, status]);

    let grid = layout.new_leaf(Style {
        size: Size {
            width: Dimension::percent(1.0),
            height: Dimension::percent(1.0),
        },
        ..Default::default()
    });
    layout.set_children(page.main, &[grid]);
    for (name, node) in [
        ("sidebar", sidebar),
        ("prompt", prompt),
        ("submit", submit),
        ("status", status),
        ("grid", grid),
    ] {
        layout.register_label(name, node);
    }
    AppLayout {
        page,
        prompt,
        submit,
        status,
        grid,
    }
}

fn grid_has_keyboard_focus(layout: &Layout, grid: NodeId, focused_id: Option<u64>) -> bool {
    focused_id == Some(layout.widget_id_keyed(grid, 0))
}

struct App {
    state: Option<AppState>,
    args: Args,
    initial_session: Option<GuiSession>,
    fatal_error: Option<anyhow::Error>,
    script: Option<ScriptRunner>,
    start_time: Option<Instant>,
    screenshot_taken: bool,
    layout_dumped: bool,
    frame_dumped: bool,
    working_directory: std::path::PathBuf,
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
    let working_directory = std::env::current_dir().context("could not read working directory")?;
    let absolute_input = if args.input.is_absolute() {
        args.input.clone()
    } else {
        working_directory.join(&args.input)
    };
    let table =
        open_table(&absolute_input).map_err(|failure| anyhow::anyhow!(failure.to_string()))?;
    if let Some(summary) = format_startup_diagnostics(&table.diagnostics) {
        eprintln!("{summary}");
    }
    let input_identity =
        InputIdentity::from_snapshot(&args.input, &absolute_input, &table.source_revision);
    let session =
        GuiSession::new(table, input_identity).context("could not adapt opened table to a grid")?;
    let event_loop = EventLoop::new().context("could not create event loop")?;
    let mut app = App {
        state: None,
        args,
        initial_session: Some(session),
        fatal_error: None,
        script,
        start_time: None,
        screenshot_taken: false,
        layout_dumped: false,
        frame_dumped: false,
        working_directory,
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
        let app_layout = build_app_layout(&mut layout);
        let Some(session) = self.initial_session.take() else {
            self.fail(event_loop, anyhow::anyhow!("table was already initialized"));
            return;
        };
        self.start_time = Some(Instant::now());
        self.state = Some(AppState {
            window,
            device,
            queue,
            surface,
            surface_config,
            core,
            layout,
            page: app_layout.page,
            prompt_node: app_layout.prompt,
            submit_node: app_layout.submit,
            status_node: app_layout.status,
            grid_node: app_layout.grid,
            grid_state: DataGridState::new(),
            selection: SelectionState::default(),
            session,
            runs_directory: self.working_directory.join(".baho/runs"),
            invocation: Invocation {
                command: "baho-gui".to_owned(),
                action: "submit".to_owned(),
                event_target: "baho_gui".to_owned(),
                arguments: std::env::args().collect(),
                working_directory: self.working_directory.clone(),
                output: None,
            },
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
        let dump_this_frame = self.args.dump_frame.is_some()
            && !self.frame_dumped
            && (self.script.is_none() || script_capture.is_some());
        if dump_this_frame {
            state.core.draw_list.start_recording();
        }
        let _prompt_response = akar_text_input(
            &mut state.core,
            &state.layout,
            state.prompt_node,
            &mut state.session.prompt,
            &mut state.session.prompt_edit,
            "Describe the result",
            true,
            &AKAR_THEME_DARK,
        );
        let submit = akar_button(
            &mut state.core,
            &state.layout,
            state.submit_node,
            "Submit",
            ButtonVariant::Solid,
            &AKAR_THEME_DARK,
        );
        if submit.clicked {
            let _ = state.session.request_submit();
        }
        let status = state.session.status.message();
        akar_paragraph(
            &mut state.core,
            &state.layout,
            state.status_node,
            &status,
            None,
            &AKAR_THEME_DARK,
        );
        let style = DataGridStyle::from_theme(&AKAR_THEME_DARK);
        let row_keys = state
            .session
            .display
            .rows()
            .iter()
            .map(|row| row.key)
            .collect::<Vec<_>>();
        let descriptors = state
            .session
            .display
            .columns()
            .iter()
            .map(|column| column.descriptor)
            .collect::<Vec<_>>();
        // Akar consumes navigation input before begin so the same frame uses
        // the updated active cell and scroll position.
        if grid_has_keyboard_focus(&state.layout, state.grid_node, state.core.input.focused_id) {
            let _ = data_grid_handle_keyboard(
                &mut state.core,
                &state.layout,
                state.grid_node,
                &mut state.grid_state,
                state.session.display.rows().len(),
                &row_keys,
                &descriptors,
                &style,
            );
        }
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
            state.session.display.rows().len(),
            &row_keys,
            style.row_height,
            style.header_height,
            &descriptors,
            &style,
        );
        data_grid_header_begin(&mut state.core, &response, &style);
        for column_index in response.visible_columns.clone() {
            if let Some(column) = state.session.display.columns().get(column_index) {
                let _ = data_grid_header_cell(
                    &mut state.core,
                    &state.layout,
                    &response,
                    state.grid_node,
                    column_index,
                    &descriptors,
                    &style,
                    &column.display_name,
                    DataGridSortDirection::None,
                );
            }
        }
        data_grid_header_end(&mut state.core);
        data_grid_body_begin(&mut state.core, &response, &row_keys, &style, &selected);
        for row_index in response.visible_rows.clone() {
            for column_index in response.visible_columns.clone() {
                let Some(row) = state.session.display.rows().get(row_index) else {
                    continue;
                };
                let Some(column) = state.session.display.columns().get(column_index) else {
                    continue;
                };
                let text = state
                    .session
                    .display
                    .cell_text(row_index, column_index)
                    .unwrap_or("");
                let cell = data_grid_cell(
                    &mut state.core,
                    &state.layout,
                    &response,
                    state.grid_node,
                    row_index,
                    row.key,
                    column_index,
                    &descriptors,
                    &style,
                    text,
                    selected.contains(&row.key),
                );
                if cell.clicked {
                    state.grid_state.active_row_key = row.key;
                    state.grid_state.active_column_key = column.descriptor.key;
                    state.grid_state.has_active_cell = true;
                    state.selection.activate(row.key, column.descriptor.key);
                    state.core.input.focused_id =
                        Some(state.layout.widget_id_keyed(state.grid_node, 0));
                }
            }
        }
        data_grid_body_end(&mut state.core);
        data_grid_end(&mut state.core);
        let processed = state.session.process_pending(
            &state.runs_directory,
            state.invocation.clone(),
            &mut state.grid_state,
            &mut state.selection,
        );
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
        if let Some(path) = self.args.dump_frame.as_ref().filter(|_| dump_this_frame) {
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
        if processed {
            state.window.request_redraw();
        }
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
    use akar_layout::{Layout, Size};
    use baho_model::{Diagnostic, Severity};

    use super::{
        SIDEBAR_WIDTH, build_app_layout, finish_event_loop, format_startup_diagnostics,
        grid_has_keyboard_focus,
    };

    #[test]
    fn app_layout_has_fixed_sidebar_and_flexible_grid_with_stable_labels() {
        let mut layout = Layout::new();
        let app = build_app_layout(&mut layout);
        layout.compute(
            app.page.root,
            (Some(800.0), Some(600.0)),
            |_, _, _, _, _| Size::ZERO,
        );

        assert_eq!(
            layout.rect(layout.resolve_label("sidebar").unwrap())[2],
            SIDEBAR_WIDTH
        );
        assert_eq!(
            layout.rect(layout.resolve_label("grid").unwrap()),
            [SIDEBAR_WIDTH, 0.0, 520.0, 600.0]
        );
        for label in ["prompt", "submit", "status"] {
            let rect = layout.rect(layout.resolve_label(label).unwrap());
            assert!(rect[2] > 0.0, "{label} has width");
            assert!(rect[3] > 0.0, "{label} has height");
        }
    }

    #[test]
    fn prompt_focus_excludes_grid_navigation_but_grid_focus_allows_it() {
        let mut layout = Layout::new();
        let app = build_app_layout(&mut layout);
        layout.compute(
            app.page.root,
            (Some(800.0), Some(600.0)),
            |_, _, _, _, _| Size::ZERO,
        );
        let prompt_focus = Some(layout.widget_id(app.prompt));
        let grid_focus = Some(layout.widget_id_keyed(app.grid, 0));

        assert!(!grid_has_keyboard_focus(&layout, app.grid, prompt_focus));
        assert!(grid_has_keyboard_focus(&layout, app.grid, grid_focus));
        assert!(!grid_has_keyboard_focus(&layout, app.grid, None));
    }

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
