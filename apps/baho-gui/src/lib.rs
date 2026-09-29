use std::{ops::Range, path::Path};

use akar_components::{DataGridAlign, DataGridColumn, DataGridState, TextEditState};
use baho_core::{
    CoreOutcome, GroundingOutcome, OpenedTable, PredicateForm, RawCell, execute_prompt,
};
use baho_ingest::{
    RowReadLimits, SelectedRow, SelectedSourceReadError, SelectedSourceReader, SourceCellValue,
};
use baho_ingest_csv::{CsvRowCheckpoint, CsvSelectedSourceReader};
use baho_model::{MaterializedView, Value};
use baho_run::{InputIdentity, Invocation, PendingRun};
use thiserror::Error;

pub const MIN_COLUMN_WIDTH: f32 = 72.0;
pub const MAX_COLUMN_WIDTH: f32 = 320.0;
pub const COLUMN_HORIZONTAL_PADDING: f32 = 24.0;
pub const WIDTH_SAMPLE_ROWS: usize = 64;
const SOURCE_OVERSCAN_ROWS: usize = 16;
const CHECKPOINT_INTERVAL: usize = 256;

pub struct SourceRangeCache {
    reader: CsvSelectedSourceReader,
    checkpoints: Vec<(usize, CsvRowCheckpoint)>,
    start: usize,
    rows: Vec<SelectedRow>,
    limits: RowReadLimits,
}

impl std::fmt::Debug for SourceRangeCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceRangeCache")
            .field("start", &self.start)
            .field("cached_rows", &self.rows.len())
            .field("checkpoints", &self.checkpoints.len())
            .finish()
    }
}

impl SourceRangeCache {
    fn new(opened: &OpenedTable) -> Result<(Self, Vec<GridRow>), SelectedSourceReadError> {
        let mut reader = opened.selected_source_reader()?;
        let max_bytes = opened
            .parser_config
            .inspection
            .max_field_size
            .saturating_mul(opened.columns.len())
            .max(1);
        let limits = RowReadLimits {
            max_rows: 1,
            max_bytes,
        };
        let mut checkpoints = vec![(0, reader.checkpoint())];
        let mut keys = Vec::with_capacity(opened.selected_row_count);
        loop {
            let batch = reader.read_next(limits)?;
            for row in batch.rows {
                let key = source_key("row", row.source_row).map_err(|error| {
                    SelectedSourceReadError::Io {
                        detail: error.to_string(),
                    }
                })?;
                keys.push(GridRow {
                    source_row: row.source_row,
                    key,
                });
                if keys.len() % CHECKPOINT_INTERVAL == 0 {
                    checkpoints.push((keys.len(), reader.checkpoint()));
                }
            }
            if batch.complete {
                break;
            }
        }
        let mut cache = Self {
            reader,
            checkpoints,
            start: 0,
            rows: Vec::new(),
            limits,
        };
        cache.reader.restore(&cache.checkpoints[0].1)?;
        Ok((cache, keys))
    }

    pub fn load(
        &mut self,
        visible: Range<usize>,
        total: usize,
    ) -> Result<(), SelectedSourceReadError> {
        if let Err(error) = self.reader.verify_source_stamp() {
            self.rows.clear();
            return Err(error);
        }
        let start = visible.start.saturating_sub(SOURCE_OVERSCAN_ROWS);
        let end = visible.end.saturating_add(SOURCE_OVERSCAN_ROWS).min(total);
        if start >= self.start && end <= self.start + self.rows.len() {
            return Ok(());
        }
        let (checkpoint_index, checkpoint) = self
            .checkpoints
            .iter()
            .rev()
            .find(|(index, _)| *index <= start)
            .expect("initial checkpoint exists");
        self.reader.restore(checkpoint)?;
        self.start = start;
        self.rows.clear();
        let mut logical = *checkpoint_index;
        while logical < end {
            let batch = self.reader.read_next(self.limits)?;
            let count = batch.rows.len();
            if logical >= start {
                self.rows.extend(batch.rows);
            }
            logical += count;
            if batch.complete || count == 0 {
                break;
            }
        }
        Ok(())
    }

    pub fn cell_text(&self, row: usize, column: usize) -> Option<&str> {
        match self
            .rows
            .get(row.checked_sub(self.start)?)?
            .cells
            .get(column)?
        {
            SourceCellValue::Present(value) => Some(value),
            SourceCellValue::Missing => Some(""),
        }
    }

    pub fn cached_row_count(&self) -> usize {
        self.rows.len()
    }
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdapterError {
    #[error("could not read selected source: {0}")]
    Source(#[from] SelectedSourceReadError),
    #[error("{kind} source index {index} cannot be represented as a grid key")]
    KeyOverflow { kind: &'static str, index: usize },
    #[error("duplicate {kind} grid key {key}")]
    DuplicateKey { kind: &'static str, key: u64 },
    #[error("materialized row {row} has no provenance")]
    MissingProvenance { row: usize },
}

fn ensure_unique_keys(
    kind: &'static str,
    keys: impl IntoIterator<Item = u64>,
) -> Result<(), AdapterError> {
    let mut seen = std::collections::HashSet::new();
    for key in keys {
        if !seen.insert(key) {
            return Err(AdapterError::DuplicateKey { kind, key });
        }
    }
    Ok(())
}

fn source_key(kind: &'static str, index: usize) -> Result<u64, AdapterError> {
    index
        .checked_add(1)
        .and_then(|key| u64::try_from(key).ok())
        .ok_or(AdapterError::KeyOverflow { kind, index })
}

fn column_key(ordinal: usize) -> Result<u64, AdapterError> {
    let ordinal = u64::try_from(ordinal).map_err(|_| AdapterError::KeyOverflow {
        kind: "column",
        index: ordinal,
    })?;
    // The high 32 bits encode the one-based source ordinal. Reject values
    // that would truncate that encoding instead of relying on integer shifts.
    if ordinal >= u32::MAX as u64 {
        return Err(AdapterError::KeyOverflow {
            kind: "column",
            index: ordinal as usize,
        });
    }
    let key = ordinal
        .checked_add(1)
        .and_then(|value| value.checked_shl(32))
        .ok_or(AdapterError::KeyOverflow {
            kind: "column",
            index: ordinal as usize,
        })?;
    Ok(key)
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridColumn {
    pub source_ordinal: usize,
    pub display_name: String,
    pub descriptor: DataGridColumn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridRow {
    pub source_row: usize,
    pub key: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleCell {
    pub row_key: u64,
    pub source_row: usize,
    pub column_key: u64,
    pub source_ordinal: usize,
    pub text: String,
    pub present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveCell {
    pub row_key: u64,
    pub column_key: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionState {
    pub selected_row_key: Option<u64>,
    pub active_cell: Option<ActiveCell>,
}

impl SelectionState {
    pub fn activate(&mut self, row_key: u64, column_key: u64) -> bool {
        let next = ActiveCell {
            row_key,
            column_key,
        };
        let changed = self.active_cell != Some(next) || self.selected_row_key != Some(row_key);
        self.selected_row_key = Some(row_key);
        self.active_cell = Some(next);
        changed
    }

    pub fn clear(&mut self) {
        self.selected_row_key = None;
        self.active_cell = None;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridAdapter {
    pub columns: Vec<GridColumn>,
    pub rows: Vec<GridRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DisplayGrid {
    Source(GridAdapter),
    Materialized(MaterializedGridAdapter),
}

impl DisplayGrid {
    pub fn columns(&self) -> &[GridColumn] {
        match self {
            Self::Source(adapter) => &adapter.columns,
            Self::Materialized(adapter) => &adapter.columns,
        }
    }

    pub fn rows(&self) -> &[GridRow] {
        match self {
            Self::Source(adapter) => &adapter.rows,
            Self::Materialized(adapter) => &adapter.rows,
        }
    }

    pub fn cell_text<'a>(
        &'a self,
        opened: &'a OpenedTable,
        row: usize,
        column: usize,
    ) -> Option<&'a str> {
        match self {
            Self::Source(adapter) => {
                let ordinal = adapter.columns.get(column)?.source_ordinal;
                adapter.cell_text(opened, row, ordinal)
            }
            Self::Materialized(adapter) => adapter.cell_text(row, column),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionStatus {
    Idle,
    Success {
        run_id: String,
    },
    NeedsClarification {
        run_id: String,
        message: String,
    },
    Failure {
        run_id: Option<String>,
        message: String,
    },
}

impl SubmissionStatus {
    pub fn message(&self) -> String {
        match self {
            Self::Idle => String::new(),
            Self::Success { run_id } => format!("Run {run_id} materialized"),
            Self::NeedsClarification { run_id, message } => {
                format!("Run {run_id} needs clarification: {message}")
            }
            Self::Failure {
                run_id: Some(run_id),
                message,
            } => format!("Run {run_id} failed: {message}"),
            Self::Failure {
                run_id: None,
                message,
            } => message.clone(),
        }
    }
}

/// Render a pending clarification the way the noninteractive CLI presents it
/// (Epic 007): every unresolved clause with its candidates, so the user can
/// re-prompt with a more specific request. A full interactive
/// clause-to-candidate resolution flow is deferred, matching the CLI.
fn render_clarification(request: &baho_core::ClarificationRequest) -> String {
    request
        .unresolved
        .iter()
        .map(|clause| {
            let candidates = clause
                .candidates
                .iter()
                .map(|candidate| {
                    let form = match candidate.predicate_form {
                        PredicateForm::EqualsValue => "equals value",
                        PredicateForm::FlagIsTrue => "flag is true",
                        PredicateForm::IsNotBlank => "is not blank",
                        PredicateForm::Compare => "compare",
                    };
                    format!("{} ({form})", candidate.display_name)
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("\"{}\" could mean: {candidates}", clause.rendered_condition)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitRequest {
    Queued,
    Blank,
    AlreadyPending,
}

#[derive(Debug)]
pub struct GuiSession {
    pub opened: OpenedTable,
    pub input_identity: InputIdentity,
    pub prompt: String,
    pub prompt_edit: TextEditState,
    pub display: DisplayGrid,
    pub source_cache: SourceRangeCache,
    pub row_keys: Vec<u64>,
    pub last_successful: Option<MaterializedView>,
    pub status: SubmissionStatus,
    pending_prompt: Option<String>,
}

impl GuiSession {
    pub fn new(opened: OpenedTable, input_identity: InputIdentity) -> Result<Self, AdapterError> {
        let (source_cache, source_rows) = SourceRangeCache::new(&opened)?;
        let mut adapter = GridAdapter::from_opened_table(&opened)?;
        adapter.rows = source_rows;
        let row_keys = adapter.rows.iter().map(|row| row.key).collect();
        let display = DisplayGrid::Source(adapter);
        Ok(Self {
            opened,
            input_identity,
            prompt: String::new(),
            prompt_edit: TextEditState::default(),
            display,
            source_cache,
            row_keys,
            last_successful: None,
            status: SubmissionStatus::Idle,
            pending_prompt: None,
        })
    }

    pub fn request_submit(&mut self) -> SubmitRequest {
        if self.pending_prompt.is_some() {
            return SubmitRequest::AlreadyPending;
        }
        if self.prompt.trim().is_empty() {
            self.status = SubmissionStatus::Failure {
                run_id: None,
                message: "A prompt is required".to_owned(),
            };
            return SubmitRequest::Blank;
        }
        self.pending_prompt = Some(self.prompt.clone());
        SubmitRequest::Queued
    }

    pub fn has_pending_submit(&self) -> bool {
        self.pending_prompt.is_some()
    }

    pub fn process_pending(
        &mut self,
        runs_directory: &Path,
        invocation: Invocation,
        grid_state: &mut DataGridState,
        selection: &mut SelectionState,
    ) -> bool {
        let Some(prompt) = self.pending_prompt.take() else {
            return false;
        };
        let pending = match PendingRun::reserve(runs_directory, invocation, &prompt) {
            Ok(pending) => pending,
            Err(error) => {
                self.status = SubmissionStatus::Failure {
                    run_id: None,
                    message: error.to_string(),
                };
                return true;
            }
        };
        let run_id = pending.id().to_owned();
        let materialized_open = baho_core::open_table_metadata(&self.opened.source_path);
        let result = match materialized_open {
            Ok(opened) if opened.source_revision != self.opened.source_revision => {
                self.status = SubmissionStatus::Failure {
                    run_id: Some(run_id),
                    message: "Source changed since opening; reopen it before running a prompt"
                        .to_owned(),
                };
                return true;
            }
            Ok(opened) => execute_prompt(&opened, &prompt),
            Err(error) => {
                self.status = SubmissionStatus::Failure {
                    run_id: Some(run_id),
                    message: error.to_string(),
                };
                return true;
            }
        };
        let next_display = result
            .output
            .as_ref()
            .map(MaterializedGridAdapter::from_view)
            .transpose();
        let recorded = pending.record_result(self.input_identity.clone(), &result);
        let recorded = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                self.status = SubmissionStatus::Failure {
                    run_id: Some(run_id),
                    message: error.to_string(),
                };
                return true;
            }
        };

        match (result.outcome, result.output, next_display) {
            (CoreOutcome::Materialized, Some(view), Ok(Some(adapter))) => {
                self.row_keys = adapter.rows.iter().map(|row| row.key).collect();
                self.display = DisplayGrid::Materialized(adapter);
                self.last_successful = Some(view);
                reset_grid_interaction(grid_state, selection);
                self.status = SubmissionStatus::Success {
                    run_id: recorded.id,
                };
            }
            (CoreOutcome::Materialized, _, Err(error)) => {
                self.status = SubmissionStatus::Failure {
                    run_id: Some(recorded.id),
                    message: error.to_string(),
                };
            }
            _ => {
                let pending_clarification =
                    result
                        .grounding
                        .as_ref()
                        .and_then(|grounding| match &grounding.outcome {
                            GroundingOutcome::NeedsClarification { request } => Some(request),
                            _ => None,
                        });
                self.status = match pending_clarification {
                    Some(request) => SubmissionStatus::NeedsClarification {
                        run_id: recorded.id,
                        message: render_clarification(request),
                    },
                    None => {
                        let message = result
                            .diagnostics
                            .iter()
                            .find(|diagnostic| {
                                matches!(diagnostic.severity, baho_model::Severity::Error)
                            })
                            .map(|diagnostic| diagnostic.message.clone())
                            .unwrap_or_else(|| "Request was not materialized".to_owned());
                        SubmissionStatus::Failure {
                            run_id: Some(recorded.id),
                            message,
                        }
                    }
                };
            }
        }
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializedCellKind {
    Missing,
    Blank,
    Text,
    Number,
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedCell {
    pub kind: MaterializedCellKind,
    pub text: String,
}

impl MaterializedCell {
    fn from_value(value: Option<&Value>) -> Self {
        match value {
            None => Self {
                kind: MaterializedCellKind::Missing,
                text: String::new(),
            },
            Some(Value::Blank) => Self {
                kind: MaterializedCellKind::Blank,
                text: String::new(),
            },
            Some(Value::Text(text)) => Self {
                kind: MaterializedCellKind::Text,
                text: text.clone(),
            },
            Some(Value::Number(number)) => Self {
                kind: MaterializedCellKind::Number,
                text: number.to_string(),
            },
            Some(Value::Boolean(value)) => Self {
                kind: MaterializedCellKind::Boolean,
                text: value.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MaterializedGridAdapter {
    pub columns: Vec<GridColumn>,
    pub rows: Vec<GridRow>,
    cells: Vec<Vec<MaterializedCell>>,
}

impl MaterializedGridAdapter {
    pub fn from_view(view: &MaterializedView) -> Result<Self, AdapterError> {
        let mut columns = Vec::with_capacity(view.columns.len());
        for column in &view.columns {
            columns.push(GridColumn {
                source_ordinal: column.ordinal,
                display_name: column.display_name.clone(),
                descriptor: DataGridColumn {
                    key: column_key(column.ordinal)?,
                    width: materialized_column_width(view, column.ordinal),
                    align: DataGridAlign::Left,
                },
            });
        }
        ensure_unique_keys("column", columns.iter().map(|column| column.descriptor.key))?;

        let mut rows = Vec::with_capacity(view.rows.len());
        for row_index in 0..view.rows.len() {
            let provenance = view
                .provenance
                .get(row_index)
                .ok_or(AdapterError::MissingProvenance { row: row_index })?;
            rows.push(GridRow {
                source_row: provenance.source_row,
                key: source_key("row", provenance.source_row)?,
            });
        }
        ensure_unique_keys("row", rows.iter().map(|row| row.key))?;

        let cells = view
            .rows
            .iter()
            .map(|row| {
                (0..columns.len())
                    .map(|column_index| {
                        MaterializedCell::from_value(
                            row.values.get(column_index).and_then(Option::as_ref),
                        )
                    })
                    .collect()
            })
            .collect();
        Ok(Self {
            columns,
            rows,
            cells,
        })
    }

    pub fn cell(&self, logical_row: usize, column_index: usize) -> Option<&MaterializedCell> {
        self.cells.get(logical_row)?.get(column_index)
    }

    pub fn cell_text(&self, logical_row: usize, column_index: usize) -> Option<&str> {
        self.cell(logical_row, column_index)
            .map(|cell| cell.text.as_str())
    }

    pub fn visible_cells(
        &self,
        visible_rows: Range<usize>,
        visible_columns: Range<usize>,
    ) -> Vec<VisibleCell> {
        let row_range =
            visible_rows.start.min(self.rows.len())..visible_rows.end.min(self.rows.len());
        let column_range = visible_columns.start.min(self.columns.len())
            ..visible_columns.end.min(self.columns.len());
        row_range
            .flat_map(|row_index| {
                column_range.clone().filter_map(move |column_index| {
                    let row = &self.rows[row_index];
                    let column = &self.columns[column_index];
                    let cell = self.cell(row_index, column_index)?;
                    Some(VisibleCell {
                        row_key: row.key,
                        source_row: row.source_row,
                        column_key: column.descriptor.key,
                        source_ordinal: column.source_ordinal,
                        text: cell.text.clone(),
                        present: cell.kind != MaterializedCellKind::Missing,
                    })
                })
            })
            .collect()
    }
}

/// Replaces grid contents as one transition and invalidates interaction state
/// that refers to the previous row and column key spaces.
pub fn reset_grid_interaction(grid_state: &mut DataGridState, selection: &mut SelectionState) {
    *grid_state = DataGridState::new();
    selection.clear();
}

impl GridAdapter {
    pub fn from_opened_table(table: &OpenedTable) -> Result<Self, AdapterError> {
        let columns = table
            .columns
            .iter()
            .map(|column| {
                Ok(GridColumn {
                    source_ordinal: column.ordinal,
                    display_name: column.display_name.clone(),
                    descriptor: DataGridColumn {
                        key: column_key(column.ordinal)?,
                        width: column_width(table, column.ordinal),
                        align: DataGridAlign::Left,
                    },
                })
            })
            .collect::<Result<Vec<_>, AdapterError>>()?;
        let rows = table
            .rows
            .iter()
            .map(|row| {
                Ok(GridRow {
                    source_row: row.source_row,
                    key: source_key("row", row.source_row)?,
                })
            })
            .collect::<Result<Vec<_>, AdapterError>>()?;
        Ok(Self { columns, rows })
    }

    pub fn cell<'a>(
        &self,
        table: &'a OpenedTable,
        logical_row: usize,
        source_ordinal: usize,
    ) -> Option<&'a RawCell> {
        let column_index = self
            .columns
            .iter()
            .position(|column| column.source_ordinal == source_ordinal)?;
        table.rows.get(logical_row)?.cells.get(column_index)
    }

    pub fn cell_text<'a>(
        &self,
        table: &'a OpenedTable,
        logical_row: usize,
        source_ordinal: usize,
    ) -> Option<&'a str> {
        match self.cell(table, logical_row, source_ordinal)? {
            RawCell::Present(text) => Some(text.as_str()),
            RawCell::Missing => Some(""),
        }
    }

    pub fn visible_cells(
        &self,
        table: &OpenedTable,
        visible_rows: Range<usize>,
        visible_columns: Range<usize>,
    ) -> Vec<VisibleCell> {
        let row_range =
            visible_rows.start.min(self.rows.len())..visible_rows.end.min(self.rows.len());
        let column_range = visible_columns.start.min(self.columns.len())
            ..visible_columns.end.min(self.columns.len());
        row_range
            .flat_map(|row_index| {
                column_range.clone().filter_map(move |column_index| {
                    let row = &self.rows[row_index];
                    let column = &self.columns[column_index];
                    let cell = self.cell(table, row_index, column.source_ordinal)?;
                    Some(VisibleCell {
                        row_key: row.key,
                        source_row: row.source_row,
                        column_key: column.descriptor.key,
                        source_ordinal: column.source_ordinal,
                        text: match cell {
                            RawCell::Present(text) => text.clone(),
                            RawCell::Missing => String::new(),
                        },
                        present: matches!(cell, RawCell::Present(_)),
                    })
                })
            })
            .collect()
    }
}

fn column_width(table: &OpenedTable, source_ordinal: usize) -> f32 {
    let header_len = table
        .columns
        .iter()
        .find(|column| column.ordinal == source_ordinal)
        .map_or(0, |column| column.display_name.chars().count());
    let sample_len = table
        .rows
        .iter()
        .take(WIDTH_SAMPLE_ROWS)
        .filter_map(|row| match row.cells.get(source_ordinal) {
            Some(RawCell::Present(text)) => Some(text.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    ((header_len.max(sample_len) as f32 * 8.0) + COLUMN_HORIZONTAL_PADDING)
        .clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
}

fn materialized_column_width(view: &MaterializedView, source_ordinal: usize) -> f32 {
    let Some((column_index, column)) = view
        .columns
        .iter()
        .enumerate()
        .find(|(_, column)| column.ordinal == source_ordinal)
    else {
        return MIN_COLUMN_WIDTH;
    };
    let sample_len = view
        .rows
        .iter()
        .take(WIDTH_SAMPLE_ROWS)
        .map(|row| {
            MaterializedCell::from_value(row.values.get(column_index).and_then(Option::as_ref))
                .text
                .chars()
                .count()
        })
        .max()
        .unwrap_or(0);
    ((column.display_name.chars().count().max(sample_len) as f32 * 8.0) + COLUMN_HORIZONTAL_PADDING)
        .clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_core::open_table;
    use baho_model::{CellAddress, ColumnDefinition, MaterializedRow, RowProvenance};
    use baho_run::Invocation;
    use std::{fs, io::Write};
    use tempfile::{NamedTempFile, TempDir};

    fn adapter_for(input: &str) -> (OpenedTable, GridAdapter) {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(input.as_bytes()).unwrap();
        let table = open_table(file.path()).unwrap();
        let adapter = GridAdapter::from_opened_table(&table).unwrap();
        (table, adapter)
    }

    fn materialized_view() -> MaterializedView {
        MaterializedView {
            columns: vec![
                ColumnDefinition {
                    id: "amount".to_string(),
                    ordinal: 7,
                    source_header_raw: None,
                    source_header_normalized: None,
                    display_name: "Amount".to_string(),
                },
                ColumnDefinition {
                    id: "flag".to_string(),
                    ordinal: 3,
                    source_header_raw: None,
                    source_header_normalized: None,
                    display_name: "Approved?".to_string(),
                },
                ColumnDefinition {
                    id: "note".to_string(),
                    ordinal: 11,
                    source_header_raw: None,
                    source_header_normalized: None,
                    display_name: "Note".to_string(),
                },
            ],
            rows: vec![
                MaterializedRow {
                    values: vec![
                        Some(Value::Number(12.5)),
                        Some(Value::Boolean(true)),
                        Some(Value::Text("ready".to_string())),
                    ],
                },
                MaterializedRow {
                    values: vec![Some(Value::Number(-0.0)), Some(Value::Blank)],
                },
            ],
            provenance: vec![
                RowProvenance {
                    source_row: 4,
                    source_addresses: vec![CellAddress {
                        sheet_index: 0,
                        row: 4,
                        col: 7,
                    }],
                },
                RowProvenance {
                    source_row: 9,
                    source_addresses: vec![CellAddress {
                        sheet_index: 0,
                        row: 9,
                        col: 7,
                    }],
                },
            ],
        }
    }

    fn session_for(input: &str) -> (TempDir, GuiSession) {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("sample.csv");
        fs::write(&path, input).unwrap();
        let opened = open_table(&path).unwrap();
        let identity = InputIdentity::from_snapshot(&path, &path, &opened.source_revision);
        let session = GuiSession::new(opened, identity).unwrap();
        (workspace, session)
    }

    fn gui_invocation(workspace: &Path) -> Invocation {
        Invocation {
            command: "baho-gui".to_owned(),
            action: "submit".to_owned(),
            event_target: "baho_gui".to_owned(),
            arguments: vec!["baho-gui".to_owned(), "sample.csv".to_owned()],
            working_directory: workspace.to_owned(),
            output: None,
        }
    }

    #[test]
    fn source_cache_bounds_rows_across_distant_and_backward_ranges() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("sample.csv");
        let mut input = String::from("Name,Value\n");
        for index in 0..1200 {
            input.push_str(&format!("item{index},{index}\n"));
        }
        fs::write(&path, input).unwrap();
        let opened = baho_core::open_table_metadata(&path).unwrap();
        assert!(opened.rows.is_empty());
        let identity = InputIdentity::from_snapshot(&path, &path, &opened.source_revision);
        let mut session = GuiSession::new(opened, identity).unwrap();
        assert_eq!(session.display.rows().len(), 1200);
        assert!(session.source_cache.checkpoint_count() > 1);
        session.source_cache.load(1050..1060, 1200).unwrap();
        assert_eq!(session.source_cache.cell_text(1050, 0), Some("item1050"));
        assert!(session.source_cache.cached_row_count() <= 42);
        session.source_cache.load(4..10, 1200).unwrap();
        assert_eq!(session.source_cache.cell_text(4, 0), Some("item4"));
        assert!(session.source_cache.cached_row_count() <= 42);
        assert_eq!(session.row_keys[1050], source_key("row", 1051).unwrap());
        fs::write(&path, "Name,Value\nchanged,1\n").unwrap();
        assert!(matches!(
            session.source_cache.load(4..10, 1200),
            Err(SelectedSourceReadError::RevisionChanged)
        ));
        assert_eq!(session.source_cache.cached_row_count(), 0);
    }

    #[test]
    fn keys_derive_from_source_coordinates() {
        let (_table, adapter) = adapter_for("Name,Amount\nAlice,10\nBob,20\n");
        assert_eq!(adapter.columns[0].descriptor.key, 1 << 32);
        assert_eq!(adapter.columns[1].descriptor.key, 2 << 32);
        assert_eq!(adapter.rows[0].key, 2);
        assert_eq!(adapter.rows[1].key, 3);
        assert_ne!(adapter.rows[0].key, adapter.rows[1].key);
    }

    #[test]
    fn raw_present_empty_and_missing_remain_distinct() {
        let (table, adapter) = adapter_for("Name,Amount\nAlice,\nBob\n");
        assert_eq!(
            adapter.cell(&table, 0, 1),
            Some(&RawCell::Present(String::new()))
        );
        assert_eq!(adapter.cell(&table, 1, 1), Some(&RawCell::Missing));
        assert_eq!(adapter.cell_text(&table, 1, 1), Some(""));
        assert!(std::ptr::eq(
            adapter.cell(&table, 0, 1).unwrap(),
            &table.rows[0].cells[1]
        ));
    }

    #[test]
    fn widths_are_deterministic_and_bounded() {
        let (_table, adapter) =
            adapter_for("Name,Notes\na,short\nb,an exceptionally long value that is capped\n");
        let (_again_table, again) =
            adapter_for("Name,Notes\na,short\nb,an exceptionally long value that is capped\n");
        let widths = adapter
            .columns
            .iter()
            .map(|c| c.descriptor.width)
            .collect::<Vec<_>>();
        assert_eq!(
            widths,
            again
                .columns
                .iter()
                .map(|c| c.descriptor.width)
                .collect::<Vec<_>>()
        );
        assert!(
            widths
                .iter()
                .all(|width| (MIN_COLUMN_WIDTH..=MAX_COLUMN_WIDTH).contains(width))
        );
        assert_eq!(widths[1], MAX_COLUMN_WIDTH);
    }

    #[test]
    fn column_keys_reject_ordinals_that_would_truncate() {
        assert!(matches!(
            column_key(u32::MAX as usize),
            Err(AdapterError::KeyOverflow { kind: "column", .. })
        ));
    }

    #[test]
    fn visible_ranges_bound_adaptation_to_requested_cells() {
        let (table, adapter) = adapter_for("A,B,C\n1,2,3\n4,5,6\n7,8,9\n");
        let cells = adapter.visible_cells(&table, 1..3, 0..2);
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[0].source_row, adapter.rows[1].source_row);
        assert_eq!(cells[0].text, "4");
        assert_eq!(cells[3].text, "8");
    }

    #[test]
    fn selection_transitions_follow_source_keys() {
        let (_table, adapter) = adapter_for("Name,Amount\nAlice,10\nBob,20\n");
        let mut selection = SelectionState::default();
        assert!(selection.activate(adapter.rows[0].key, adapter.columns[0].descriptor.key));
        assert_eq!(selection.selected_row_key, Some(adapter.rows[0].key));
        assert!(selection.activate(adapter.rows[1].key, adapter.columns[1].descriptor.key));
        assert_eq!(selection.active_cell.unwrap().row_key, adapter.rows[1].key);
        selection.clear();
        assert_eq!(selection, SelectionState::default());
    }

    #[test]
    fn materialized_adapter_preserves_order_names_and_formats_values() {
        let adapter = MaterializedGridAdapter::from_view(&materialized_view()).unwrap();

        assert_eq!(
            adapter
                .columns
                .iter()
                .map(|column| (column.source_ordinal, column.display_name.as_str()))
                .collect::<Vec<_>>(),
            vec![(7, "Amount"), (3, "Approved?"), (11, "Note")]
        );
        assert_eq!(adapter.cell_text(0, 0), Some("12.5"));
        assert_eq!(adapter.cell_text(0, 1), Some("true"));
        assert_eq!(adapter.cell_text(0, 2), Some("ready"));
        assert_eq!(adapter.cell_text(1, 0), Some("-0"));
        assert_eq!(
            adapter.cell(1, 1).unwrap().kind,
            MaterializedCellKind::Blank
        );
        assert_eq!(adapter.cell_text(1, 1), Some(""));
        assert_eq!(
            adapter.cell(1, 2).unwrap().kind,
            MaterializedCellKind::Missing
        );
        assert_eq!(adapter.cell_text(1, 2), Some(""));
    }

    #[test]
    fn materialized_keys_are_provenance_derived_stable_and_unique() {
        let view = materialized_view();
        let adapter = MaterializedGridAdapter::from_view(&view).unwrap();
        let again = MaterializedGridAdapter::from_view(&view).unwrap();

        assert_eq!(adapter.rows[0].key, source_key("row", 4).unwrap());
        assert_eq!(adapter.rows[1].key, source_key("row", 9).unwrap());
        assert_eq!(adapter.rows, again.rows);
        assert_ne!(adapter.rows[0].key, adapter.rows[1].key);
        assert_ne!(
            adapter.columns[0].descriptor.key,
            adapter.columns[1].descriptor.key
        );
    }

    #[test]
    fn materialized_adapter_rejects_key_collisions_and_missing_provenance() {
        let mut duplicate_column = materialized_view();
        duplicate_column.columns[1].ordinal = duplicate_column.columns[0].ordinal;
        assert!(matches!(
            MaterializedGridAdapter::from_view(&duplicate_column),
            Err(AdapterError::DuplicateKey { kind: "column", .. })
        ));

        let mut duplicate_row = materialized_view();
        duplicate_row.provenance[1].source_row = duplicate_row.provenance[0].source_row;
        assert!(matches!(
            MaterializedGridAdapter::from_view(&duplicate_row),
            Err(AdapterError::DuplicateKey { kind: "row", .. })
        ));

        let mut missing_provenance = materialized_view();
        missing_provenance.provenance.pop();
        assert_eq!(
            MaterializedGridAdapter::from_view(&missing_provenance).unwrap_err(),
            AdapterError::MissingProvenance { row: 1 }
        );
    }

    #[test]
    fn materialized_visible_ranges_only_adapt_requested_cells() {
        let adapter = MaterializedGridAdapter::from_view(&materialized_view()).unwrap();
        let cells = adapter.visible_cells(1..20, 1..20);

        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].source_row, 9);
        assert_eq!(cells[0].source_ordinal, 3);
        assert!(cells[0].present);
        assert_eq!(cells[1].source_ordinal, 11);
        assert!(!cells[1].present);
    }

    #[test]
    fn replacing_an_adapter_resets_grid_navigation_and_selection() {
        let mut grid_state = DataGridState {
            scroll_x: 80.0,
            scroll_y: 160.0,
            active_row_key: 5,
            active_column_key: 8,
            has_active_cell: true,
        };
        let mut selection = SelectionState::default();
        selection.activate(5, 8);

        reset_grid_interaction(&mut grid_state, &mut selection);

        assert_eq!(grid_state.scroll_x, 0.0);
        assert_eq!(grid_state.scroll_y, 0.0);
        assert!(!grid_state.has_active_cell);
        assert_eq!(selection, SelectionState::default());
    }

    #[test]
    fn blank_prompt_is_rejected_before_a_run_is_reserved() {
        let (workspace, mut session) = session_for("Name\nAda\n");
        session.prompt = " \t\n".to_owned();

        assert_eq!(session.request_submit(), SubmitRequest::Blank);
        assert!(!session.has_pending_submit());
        assert_eq!(session.status.message(), "A prompt is required");
        assert!(!workspace.path().join(".baho/runs").exists());
    }

    #[test]
    fn one_submit_edge_is_consumed_once_and_preserves_exact_prompt() {
        let (workspace, mut session) = session_for("Name\nAda\n");
        session.prompt = "  List Name\n".to_owned();
        assert_eq!(session.request_submit(), SubmitRequest::Queued);
        assert_eq!(session.request_submit(), SubmitRequest::AlreadyPending);

        let mut grid_state = DataGridState::new();
        let mut selection = SelectionState::default();
        let runs = workspace.path().join(".baho/runs");
        assert!(session.process_pending(
            &runs,
            gui_invocation(workspace.path()),
            &mut grid_state,
            &mut selection,
        ));
        assert!(!session.process_pending(
            &runs,
            gui_invocation(workspace.path()),
            &mut grid_state,
            &mut selection,
        ));

        assert_eq!(
            fs::read_to_string(runs.join("000001/intent.txt")).unwrap(),
            "  List Name\n"
        );
        assert!(!runs.join("000002").exists());
        assert_eq!(session.prompt, "  List Name\n");
        assert_eq!(session.status.message(), "Run 000001 materialized");
    }

    #[test]
    fn failure_preserves_the_last_successful_display() {
        let (workspace, mut session) = session_for("Name,City\nAda,London\nLin,Paris\n");
        let runs = workspace.path().join(".baho/runs");
        let mut grid_state = DataGridState::new();
        let mut selection = SelectionState::default();

        session.prompt = "List Name".to_owned();
        assert_eq!(session.request_submit(), SubmitRequest::Queued);
        session.process_pending(
            &runs,
            gui_invocation(workspace.path()),
            &mut grid_state,
            &mut selection,
        );
        let successful = session.last_successful.clone().unwrap();

        session.prompt = "Calculate an average".to_owned();
        assert_eq!(session.request_submit(), SubmitRequest::Queued);
        session.process_pending(
            &runs,
            gui_invocation(workspace.path()),
            &mut grid_state,
            &mut selection,
        );

        assert_eq!(session.last_successful.as_ref(), Some(&successful));
        assert_eq!(
            session.display.cell_text(&session.opened, 0, 0),
            Some("Ada")
        );
        assert!(
            matches!(session.status, SubmissionStatus::Failure { run_id: Some(ref id), .. } if id == "000002")
        );
        assert!(runs.join("000002/manifest.json").is_file());
    }

    #[test]
    fn repeated_requests_use_the_original_opened_snapshot() {
        let (workspace, mut session) = session_for("Name,City\nAda,London\nLin,Paris\n");
        let runs = workspace.path().join(".baho/runs");
        let mut grid_state = DataGridState::new();
        let mut selection = SelectionState::default();

        for prompt in ["List Name", "List City"] {
            session.prompt = prompt.to_owned();
            assert_eq!(session.request_submit(), SubmitRequest::Queued);
            session.process_pending(
                &runs,
                gui_invocation(workspace.path()),
                &mut grid_state,
                &mut selection,
            );
        }

        assert_eq!(session.display.columns()[0].display_name, "City");
        assert_eq!(
            session.display.cell_text(&session.opened, 0, 0),
            Some("London")
        );
        assert!(runs.join("000001").is_dir());
        assert!(runs.join("000002").is_dir());
    }

    #[test]
    fn ambiguous_row_filter_reports_clarification_instead_of_a_generic_failure() {
        let (workspace, mut session) = session_for(
            "ID,Job,Note,Amount\n1,unemployed,other,5\n2,employed,unemployed,20\n3,employed,other,1\n",
        );
        let runs = workspace.path().join(".baho/runs");
        let mut grid_state = DataGridState::new();
        let mut selection = SelectionState::default();

        session.prompt = "List rows where unemployed and < 10".to_owned();
        assert_eq!(session.request_submit(), SubmitRequest::Queued);
        session.process_pending(
            &runs,
            gui_invocation(workspace.path()),
            &mut grid_state,
            &mut selection,
        );

        match &session.status {
            SubmissionStatus::NeedsClarification { run_id, message } => {
                assert_eq!(run_id, "000001");
                assert!(message.contains("unemployed"), "{message}");
                assert!(message.contains("Amount (compare)"), "{message}");
            }
            other => panic!("expected a clarification status, got {other:?}"),
        }
        assert!(session.status.message().contains("needs clarification"));
        // The source table remains displayed; nothing was materialized.
        assert!(matches!(session.display, DisplayGrid::Source(_)));
    }
}
