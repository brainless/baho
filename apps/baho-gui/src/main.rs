use std::sync::Arc;
use std::time::{Duration, Instant};

use akar_components::{
    AKAR_THEME_DARK, ButtonVariant, DataGridSortDirection, DataGridState, DataGridStyle,
    akar_button, akar_paragraph, akar_text_input, data_grid_begin, data_grid_body_begin,
    data_grid_body_end, data_grid_cell, data_grid_end, data_grid_handle_keyboard,
    data_grid_header_begin, data_grid_header_cell, data_grid_header_end,
};
use akar_core::{AkarCore, QuadCall, Z_BASE};
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

use baho_gui::{GuiSession, SelectionState, SubmissionStatus, SubmitRequest};
mod script;
use script::{ScriptRunner, parse_script};

#[derive(Debug, Parser)]
#[command(name = "baho-gui", about = "View a selected baho table")]
struct Args {
    /// A CSV file to open at startup. When omitted, drop a file onto the
    /// window to open it.
    input: Option<std::path::PathBuf>,
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
    history_node: NodeId,
    status_node: NodeId,
    grid_container_node: NodeId,
    grid_node: NodeId,
    grid_state: DataGridState,
    selection: SelectionState,
    session: Option<GuiSession>,
    empty_state_message: String,
    /// Prompts the user has typed, kept in memory only and keyed by the
    /// absolute source path so switching files (via drag-and-drop) does not
    /// mix histories or require persistence.
    history: std::collections::BTreeMap<std::path::PathBuf, Vec<String>>,
    current_input_path: Option<std::path::PathBuf>,
    runs_directory: std::path::PathBuf,
    invocation: Invocation,
}

const SIDEBAR_WIDTH: f32 = 280.0;
const GRID_OUTER_MARGIN: f32 = 12.0;
const GRID_CONTAINER_PADDING: f32 = 8.0;
const GRID_CONTAINER_RADIUS: f32 = 10.0;

struct AppLayout {
    page: PageLayout,
    prompt: NodeId,
    submit: NodeId,
    history: NodeId,
    status: NodeId,
    grid_container: NodeId,
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
    let history = layout.new_leaf(Style {
        flex_grow: 1.0,
        flex_shrink: 1.0,
        size: Size {
            width: Dimension::percent(1.0),
            height: length(0.0_f32),
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
    layout.set_children(sidebar, &[history, prompt, submit, status]);

    layout.set_padding(
        page.main,
        GRID_OUTER_MARGIN,
        GRID_OUTER_MARGIN,
        GRID_OUTER_MARGIN,
        GRID_OUTER_MARGIN,
    );
    let grid_container = layout.new_leaf(Style {
        display: Display::Flex,
        flex_direction: FlexDirection::Column,
        size: Size {
            width: Dimension::percent(1.0),
            height: Dimension::percent(1.0),
        },
        ..Default::default()
    });
    layout.set_padding(
        grid_container,
        GRID_CONTAINER_PADDING,
        GRID_CONTAINER_PADDING,
        GRID_CONTAINER_PADDING,
        GRID_CONTAINER_PADDING,
    );
    let grid = layout.new_leaf(Style {
        size: Size {
            width: Dimension::percent(1.0),
            height: Dimension::percent(1.0),
        },
        ..Default::default()
    });
    layout.set_children(grid_container, &[grid]);
    layout.set_children(page.main, &[grid_container]);
    for (name, node) in [
        ("sidebar", sidebar),
        ("prompt", prompt),
        ("submit", submit),
        ("history", history),
        ("status", status),
        ("grid_container", grid_container),
        ("grid", grid),
    ] {
        layout.register_label(name, node);
    }
    AppLayout {
        page,
        prompt,
        submit,
        history,
        status,
        grid_container,
        grid,
    }
}

fn theme_color(color: u32) -> [f32; 4] {
    [
        ((color >> 24) & 0xff) as f32 / 255.0,
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
    ]
}

fn grid_container_quad(rect: [f32; 4]) -> QuadCall {
    QuadCall {
        rect,
        fill: theme_color(AKAR_THEME_DARK.base_200),
        border_color: theme_color(AKAR_THEME_DARK.base_300),
        corner_radii: [GRID_CONTAINER_RADIUS; 4],
        border_width: 1.0,
        z: Z_BASE,
        shadow_blur: 0.0,
        shadow_spread: 0.0,
        shadow_color: [0.0; 4],
        shadow_offset: [0.0; 2],
        _pad: [0.0; 2],
    }
}

fn grid_has_keyboard_focus(layout: &Layout, grid: NodeId, focused_id: Option<u64>) -> bool {
    focused_id == Some(layout.widget_id_keyed(grid, 0))
}

const HISTORY_PATH_BUDGET: usize = 36;

/// Shortens a path for a narrow sidebar while always keeping the file name
/// fully visible, e.g. `…/deep/nested/report.csv`.
fn truncate_path_for_display(path: &std::path::Path, budget: usize) -> String {
    let full = path.display().to_string();
    if full.chars().count() <= budget {
        return full;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&full);
    if file_name.chars().count() + 1 >= budget {
        return format!("…{}", tail_chars(file_name, budget.saturating_sub(1)));
    }
    let remaining = budget - file_name.chars().count() - 1;
    let head = tail_chars(&full[..full.len() - file_name.len()], remaining);
    format!("…{head}{file_name}")
}

fn tail_chars(text: &str, count: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(count);
    chars[start..].iter().collect()
}

/// Renders the sidebar history panel text for `path`: a truncated header
/// followed by every prompt recorded for it, most recent first. History is
/// in-memory only (Epic: GUI drag-and-drop follow-up) and keyed by absolute
/// path, so switching files never mixes histories.
fn render_history_panel(
    path: Option<&std::path::Path>,
    history: &std::collections::BTreeMap<std::path::PathBuf, Vec<String>>,
) -> String {
    let Some(path) = path else {
        return String::new();
    };
    let header = truncate_path_for_display(path, HISTORY_PATH_BUDGET);
    let entries = history.get(path).map(Vec::as_slice).unwrap_or_default();
    if entries.is_empty() {
        return format!("{header}\nNo searches yet");
    }
    let mut lines = vec![header];
    lines.extend(
        entries
            .iter()
            .rev()
            .enumerate()
            .map(|(index, prompt)| format!("{}. {}", index + 1, prompt)),
    );
    lines.join("\n")
}

struct App {
    state: Option<AppState>,
    args: Args,
    initial_session: Option<GuiSession>,
    initial_input_path: Option<std::path::PathBuf>,
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

const DEFAULT_EMPTY_STATE_MESSAGE: &str = "Drop a CSV file here to open it";

/// Opens `input` (relative paths are resolved against `working_directory`)
/// and adapts it into a fresh `GuiSession`. Shared by startup and by
/// drag-and-drop, which both need to turn a path into a ready session.
fn open_session(
    input: &std::path::Path,
    working_directory: &std::path::Path,
) -> Result<(GuiSession, Vec<Diagnostic>, std::path::PathBuf)> {
    let absolute_input = if input.is_absolute() {
        input.to_path_buf()
    } else {
        working_directory.join(input)
    };
    let table =
        open_table(&absolute_input).map_err(|failure| anyhow::anyhow!(failure.to_string()))?;
    let diagnostics = table.diagnostics.clone();
    let input_identity =
        InputIdentity::from_snapshot(input, &absolute_input, &table.source_revision);
    let session =
        GuiSession::new(table, input_identity).context("could not adapt opened table to a grid")?;
    Ok((session, diagnostics, absolute_input))
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
    let (initial_session, initial_input_path) = match &args.input {
        Some(input) => {
            let (session, diagnostics, absolute_input) = open_session(input, &working_directory)?;
            if let Some(summary) = format_startup_diagnostics(&diagnostics) {
                eprintln!("{summary}");
            }
            (Some(session), Some(absolute_input))
        }
        None => (None, None),
    };
    let event_loop = EventLoop::new().context("could not create event loop")?;
    let mut app = App {
        state: None,
        args,
        initial_session,
        initial_input_path,
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
        let title = match &self.args.input {
            Some(input) => format!(
                "baho — {}",
                input
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("table")
            ),
            None => "baho — drop a CSV file to begin".to_owned(),
        };
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
        let session = self.initial_session.take();
        let current_input_path = self.initial_input_path.take();
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
            history_node: app_layout.history,
            status_node: app_layout.status,
            grid_container_node: app_layout.grid_container,
            grid_node: app_layout.grid,
            grid_state: DataGridState::new(),
            selection: SelectionState::default(),
            session,
            empty_state_message: DEFAULT_EMPTY_STATE_MESSAGE.to_owned(),
            history: std::collections::BTreeMap::new(),
            current_input_path,
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
        // The unpositioned drops queue is drained here, before the next
        // redraw's `AkarCore::begin_frame` clears it (winit reports drops
        // without a cursor position, so they are window-level, not
        // targeted at a layout node).
        if !state.core.input.unpositioned_file_drops.is_empty() {
            let drops = std::mem::take(&mut state.core.input.unpositioned_file_drops);
            if let Some(path) = drops.into_iter().flatten().next_back() {
                self.load_dropped_file(path);
            }
        }
        let Some(state) = self.state.as_mut() else {
            return;
        };
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

    /// Opens a dropped file as a fresh session, replacing whatever table (if
    /// any) was already open. A failed open never tears down an existing
    /// session; it reports the error where the user is currently looking.
    fn load_dropped_file(&mut self, path: std::path::PathBuf) {
        let working_directory = self.working_directory.clone();
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match open_session(&path, &working_directory) {
            Ok((session, diagnostics, absolute_input)) => {
                if let Some(summary) = format_startup_diagnostics(&diagnostics) {
                    eprintln!("{summary}");
                }
                let title = format!(
                    "baho — {}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("table")
                );
                state.window.set_title(&title);
                state.grid_state = DataGridState::new();
                state.selection = SelectionState::default();
                state.session = Some(session);
                state.current_input_path = Some(absolute_input);
            }
            Err(error) => {
                let message = format!("Could not open {}: {error:#}", path.display());
                match state.session.as_mut() {
                    Some(session) => {
                        session.status = SubmissionStatus::Failure {
                            run_id: None,
                            message,
                        }
                    }
                    None => state.empty_state_message = message,
                }
            }
        }
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
        let mut dumped_visible: Option<(std::ops::Range<usize>, std::ops::Range<usize>)> = None;
        let processed = if let Some(session) = state.session.as_mut() {
            let _prompt_response = akar_text_input(
                &mut state.core,
                &state.layout,
                state.prompt_node,
                &mut session.prompt,
                &mut session.prompt_edit,
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
                let prompt = session.prompt.trim().to_owned();
                if session.request_submit() == SubmitRequest::Queued {
                    if let Some(path) = &state.current_input_path {
                        state.history.entry(path.clone()).or_default().push(prompt);
                    }
                }
            }
            let status = session.status.message();
            akar_paragraph(
                &mut state.core,
                &state.layout,
                state.status_node,
                &status,
                None,
                &AKAR_THEME_DARK,
            );
            let history_text =
                render_history_panel(state.current_input_path.as_deref(), &state.history);
            akar_paragraph(
                &mut state.core,
                &state.layout,
                state.history_node,
                &history_text,
                None,
                &AKAR_THEME_DARK,
            );
            let container_rect = state.layout.rect(state.grid_container_node);
            if container_rect[2] > 0.0 && container_rect[3] > 0.0 {
                state
                    .core
                    .draw_list
                    .push_quad(grid_container_quad(container_rect));
            }
            let style = DataGridStyle::from_theme(&AKAR_THEME_DARK);
            let row_keys = session
                .display
                .rows()
                .iter()
                .map(|row| row.key)
                .collect::<Vec<_>>();
            let descriptors = session
                .display
                .columns()
                .iter()
                .map(|column| column.descriptor)
                .collect::<Vec<_>>();
            // Akar consumes navigation input before begin so the same frame uses
            // the updated active cell and scroll position.
            if grid_has_keyboard_focus(&state.layout, state.grid_node, state.core.input.focused_id)
            {
                let _ = data_grid_handle_keyboard(
                    &mut state.core,
                    &state.layout,
                    state.grid_node,
                    &mut state.grid_state,
                    session.display.rows().len(),
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
                session.display.rows().len(),
                &row_keys,
                style.row_height,
                style.header_height,
                &descriptors,
                &style,
            );
            data_grid_header_begin(&mut state.core, &response, &style);
            for column_index in response.visible_columns.clone() {
                if let Some(column) = session.display.columns().get(column_index) {
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
                    let Some(row) = session.display.rows().get(row_index) else {
                        continue;
                    };
                    let Some(column) = session.display.columns().get(column_index) else {
                        continue;
                    };
                    let text = session
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
            dumped_visible = Some((
                response.visible_rows.clone(),
                response.visible_columns.clone(),
            ));
            session.process_pending(
                &state.runs_directory,
                state.invocation.clone(),
                &mut state.grid_state,
                &mut state.selection,
            )
        } else {
            akar_paragraph(
                &mut state.core,
                &state.layout,
                state.status_node,
                &state.empty_state_message,
                None,
                &AKAR_THEME_DARK,
            );
            let container_rect = state.layout.rect(state.grid_container_node);
            if container_rect[2] > 0.0 && container_rect[3] > 0.0 {
                state
                    .core
                    .draw_list
                    .push_quad(grid_container_quad(container_rect));
            }
            akar_paragraph(
                &mut state.core,
                &state.layout,
                state.grid_node,
                "Drop a CSV file here to open it",
                None,
                &AKAR_THEME_DARK,
            );
            false
        };
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
            let (visible_rows, visible_columns) = dumped_visible.unwrap_or((0..0, 0..0));
            let dump = serde_json::json!({ "recorded_calls": state.core.draw_list.recorded_calls(), "labeled_rects": state.layout.labeled_rects(), "visible_rows": visible_rows, "visible_columns": visible_columns });
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
    use clap::Parser;

    use super::{
        Args, GRID_CONTAINER_RADIUS, SIDEBAR_WIDTH, build_app_layout, finish_event_loop,
        format_startup_diagnostics, grid_container_quad, grid_has_keyboard_focus, open_session,
        render_history_panel, truncate_path_for_display,
    };

    #[test]
    fn app_layout_insets_grid_inside_main_content() {
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
            layout.rect(layout.resolve_label("grid_container").unwrap()),
            [SIDEBAR_WIDTH + 12.0, 12.0, 496.0, 576.0]
        );
        assert_eq!(
            layout.rect(layout.resolve_label("grid").unwrap()),
            [SIDEBAR_WIDTH + 20.0, 20.0, 480.0, 560.0]
        );
        for label in ["prompt", "submit", "history", "status"] {
            let rect = layout.rect(layout.resolve_label(label).unwrap());
            assert!(rect[2] > 0.0, "{label} has width");
            assert!(rect[3] > 0.0, "{label} has height");
        }
        // History sits at the top of the sidebar and grows to fill the
        // remaining space; the prompt, submit, and status sit at the bottom.
        let history_rect = layout.rect(layout.resolve_label("history").unwrap());
        let prompt_rect = layout.rect(layout.resolve_label("prompt").unwrap());
        let submit_rect = layout.rect(layout.resolve_label("submit").unwrap());
        let status_rect = layout.rect(layout.resolve_label("status").unwrap());
        assert_eq!(history_rect[1], 16.0);
        assert!(prompt_rect[1] > history_rect[1]);
        assert!(submit_rect[1] > prompt_rect[1]);
        assert!(status_rect[1] > submit_rect[1]);
    }

    #[test]
    fn grid_container_has_four_rounded_corners() {
        let quad = grid_container_quad([292.0, 12.0, 496.0, 576.0]);
        assert_eq!(quad.corner_radii, [GRID_CONTAINER_RADIUS; 4]);
        assert!(quad.border_width > 0.0);
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

    #[test]
    fn input_argument_is_optional_so_the_gui_can_start_empty() {
        let without_input = Args::try_parse_from(["baho-gui"]).unwrap();
        assert!(without_input.input.is_none());

        let with_input = Args::try_parse_from(["baho-gui", "table.csv"]).unwrap();
        assert_eq!(
            with_input.input.unwrap(),
            std::path::PathBuf::from("table.csv")
        );
    }

    #[test]
    fn open_session_adapts_a_relative_path_against_the_working_directory() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("table.csv"), "Name\nAda\n").unwrap();

        let (session, diagnostics, absolute_input) =
            open_session(std::path::Path::new("table.csv"), workspace.path()).unwrap();

        assert!(diagnostics.is_empty());
        assert_eq!(session.display.columns()[0].display_name, "Name");
        assert_eq!(absolute_input, workspace.path().join("table.csv"));
    }

    #[test]
    fn open_session_reports_an_error_for_an_unreadable_path() {
        let workspace = tempfile::tempdir().unwrap();
        let error = open_session(std::path::Path::new("missing.csv"), workspace.path())
            .expect_err("a missing file cannot be opened");
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn short_paths_are_shown_in_full() {
        let path = std::path::Path::new("/tmp/report.csv");
        assert_eq!(truncate_path_for_display(path, 36), "/tmp/report.csv");
    }

    #[test]
    fn long_paths_are_truncated_but_keep_the_file_name() {
        let path = std::path::Path::new("/Users/example/Projects/baho/data/very/deep/report.csv");
        let truncated = truncate_path_for_display(path, 30);
        assert!(truncated.chars().count() <= 30, "{truncated}");
        assert!(truncated.starts_with('…'));
        assert!(truncated.ends_with("report.csv"));
    }

    #[test]
    fn a_file_name_alone_longer_than_the_budget_is_never_dropped() {
        let path =
            std::path::Path::new("/an-exceptionally-long-file-name-that-alone-exceeds-budget.csv");
        let truncated = truncate_path_for_display(path, 20);
        assert!(truncated.ends_with(".csv"));
    }

    #[test]
    fn history_panel_is_empty_without_an_open_file() {
        assert_eq!(render_history_panel(None, &Default::default()), "");
    }

    #[test]
    fn history_panel_shows_the_header_even_with_no_searches_yet() {
        let path = std::path::PathBuf::from("/tmp/report.csv");
        let panel = render_history_panel(Some(&path), &Default::default());
        assert_eq!(panel, "/tmp/report.csv\nNo searches yet");
    }

    #[test]
    fn history_panel_lists_prompts_most_recent_first_for_the_current_file() {
        let path = std::path::PathBuf::from("/tmp/report.csv");
        let other = std::path::PathBuf::from("/tmp/other.csv");
        let mut history = std::collections::BTreeMap::new();
        history.insert(
            path.clone(),
            vec!["List name".to_owned(), "List city".to_owned()],
        );
        history.insert(other, vec!["List unrelated".to_owned()]);

        let panel = render_history_panel(Some(&path), &history);

        assert_eq!(panel, "/tmp/report.csv\n1. List city\n2. List name");
    }
}
