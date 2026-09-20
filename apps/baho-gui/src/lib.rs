use std::ops::Range;

use akar_components::{DataGridAlign, DataGridColumn};
use baho_core::{OpenedTable, RawCell};
use thiserror::Error;

pub const MIN_COLUMN_WIDTH: f32 = 72.0;
pub const MAX_COLUMN_WIDTH: f32 = 320.0;
pub const COLUMN_HORIZONTAL_PADDING: f32 = 24.0;
pub const WIDTH_SAMPLE_ROWS: usize = 64;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdapterError {
    #[error("{kind} source index {index} cannot be represented as a grid key")]
    KeyOverflow { kind: &'static str, index: usize },
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
    cells: Vec<Vec<RawCell>>,
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
        let cells = table.rows.iter().map(|row| row.cells.clone()).collect();

        Ok(Self {
            columns,
            rows,
            cells,
        })
    }

    pub fn cell(&self, logical_row: usize, source_ordinal: usize) -> Option<&RawCell> {
        let column_index = self
            .columns
            .iter()
            .position(|column| column.source_ordinal == source_ordinal)?;
        self.cells.get(logical_row)?.get(column_index)
    }

    pub fn cell_text(&self, logical_row: usize, source_ordinal: usize) -> Option<&str> {
        match self.cell(logical_row, source_ordinal)? {
            RawCell::Present(text) => Some(text.as_str()),
            RawCell::Missing => Some(""),
        }
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
                    let cell = self.cell(row_index, column.source_ordinal)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use baho_core::open_table;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn adapter_for(input: &str) -> GridAdapter {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(input.as_bytes()).unwrap();
        let table = open_table(file.path()).unwrap();
        GridAdapter::from_opened_table(&table).unwrap()
    }

    #[test]
    fn keys_derive_from_source_coordinates() {
        let adapter = adapter_for("Name,Amount\nAlice,10\nBob,20\n");
        assert_eq!(adapter.columns[0].descriptor.key, 1 << 32);
        assert_eq!(adapter.columns[1].descriptor.key, 2 << 32);
        assert_eq!(adapter.rows[0].key, 2);
        assert_eq!(adapter.rows[1].key, 3);
        assert_ne!(adapter.rows[0].key, adapter.rows[1].key);
    }

    #[test]
    fn raw_present_empty_and_missing_remain_distinct() {
        let adapter = adapter_for("Name,Amount\nAlice,\nBob\n");
        assert_eq!(adapter.cell(0, 1), Some(&RawCell::Present(String::new())));
        assert_eq!(adapter.cell(1, 1), Some(&RawCell::Missing));
        assert_eq!(adapter.cell_text(1, 1), Some(""));
    }

    #[test]
    fn widths_are_deterministic_and_bounded() {
        let adapter =
            adapter_for("Name,Notes\na,short\nb,an exceptionally long value that is capped\n");
        let again =
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
        let adapter = adapter_for("A,B,C\n1,2,3\n4,5,6\n7,8,9\n");
        let cells = adapter.visible_cells(1..3, 0..2);
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[0].source_row, adapter.rows[1].source_row);
        assert_eq!(cells[0].text, "4");
        assert_eq!(cells[3].text, "8");
    }

    #[test]
    fn selection_transitions_follow_source_keys() {
        let adapter = adapter_for("Name,Amount\nAlice,10\nBob,20\n");
        let mut selection = SelectionState::default();
        assert!(selection.activate(adapter.rows[0].key, adapter.columns[0].descriptor.key));
        assert_eq!(selection.selected_row_key, Some(adapter.rows[0].key));
        assert!(selection.activate(adapter.rows[1].key, adapter.columns[1].descriptor.key));
        assert_eq!(selection.active_cell.unwrap().row_key, adapter.rows[1].key);
        selection.clear();
        assert_eq!(selection, SelectionState::default());
    }
}
