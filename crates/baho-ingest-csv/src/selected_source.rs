//! Sequential, bounded row access for a previously selected CSV table.

use std::fs::{self, File, Metadata};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use baho_ingest::{
    RowReadLimits, SelectedRow, SelectedRowBatch, SelectedSourceMetadata, SelectedSourceReadError,
    SelectedSourceReader, SourceCellValue,
};
use baho_model::revision::SourceRevision;
use sha2::{Digest, Sha256};

use crate::config::ParserConfig;

/// A sequential reader for the data rows inside a selected CSV region.
pub struct CsvSelectedSourceReader {
    path: PathBuf,
    config: ParserConfig,
    metadata: SelectedSourceMetadata,
    stamp: FileStamp,
    reader: csv::Reader<BufReader<File>>,
    next_source_row: usize,
    blank_count: usize,
    incompatible_count: usize,
    finished: bool,
    pending: Option<SelectedRow>,
}

/// Logical-record checkpoint tied to the reader's verified source revision.
#[derive(Clone)]
pub struct CsvRowCheckpoint {
    position: csv::Position,
    next_source_row: usize,
    blank_count: usize,
    incompatible_count: usize,
}

#[derive(Clone, Copy)]
struct FileStamp {
    size: u64,
    modified: Option<SystemTime>,
}

impl From<Metadata> for FileStamp {
    fn from(metadata: Metadata) -> Self {
        Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

impl CsvSelectedSourceReader {
    pub fn verify_source_stamp(&self) -> Result<(), SelectedSourceReadError> {
        verify_stamp(&self.path, self.stamp)
    }
    pub fn checkpoint(&self) -> CsvRowCheckpoint {
        CsvRowCheckpoint {
            position: self.reader.position().clone(),
            next_source_row: self.next_source_row,
            blank_count: self.blank_count,
            incompatible_count: self.incompatible_count,
        }
    }

    pub fn restore(
        &mut self,
        checkpoint: &CsvRowCheckpoint,
    ) -> Result<(), SelectedSourceReadError> {
        verify_stamp(&self.path, self.stamp)?;
        self.reader
            .seek(checkpoint.position.clone())
            .map_err(|error| SelectedSourceReadError::Io {
                detail: error.to_string(),
            })?;
        self.next_source_row = checkpoint.next_source_row;
        self.blank_count = checkpoint.blank_count;
        self.incompatible_count = checkpoint.incompatible_count;
        self.finished = false;
        self.pending = None;
        Ok(())
    }
    /// Open a reader for a selected table. The source is verified against the
    /// revision at open. Batch reads check the file stamp, and the full hash
    /// is verified again before reporting completion.
    pub fn open(
        path: &Path,
        metadata: SelectedSourceMetadata,
        config: ParserConfig,
    ) -> Result<Self, SelectedSourceReadError> {
        verify_revision(path, &metadata.source_revision)?;
        let stamp = fs::metadata(path).map_err(io_error)?.into();
        let file = File::open(path).map_err(io_error)?;
        let reader = csv::ReaderBuilder::new()
            .delimiter(config.dialect.delimiter)
            .quote(config.dialect.quote)
            .escape(Some(config.dialect.quote_escape))
            .has_headers(false)
            .flexible(true)
            .from_reader(BufReader::new(file));
        Ok(Self {
            path: path.to_path_buf(),
            config,
            metadata,
            stamp,
            reader,
            next_source_row: 0,
            blank_count: 0,
            incompatible_count: 0,
            finished: false,
            pending: None,
        })
    }

    fn read_data_row(&mut self) -> Result<Option<SelectedRow>, SelectedSourceReadError> {
        let width = self.metadata.columns.len();
        loop {
            if self.next_source_row > self.metadata.region.body_end_row {
                self.finished = true;
                return Ok(None);
            }
            let source_row = self.next_source_row;
            let mut record = csv::StringRecord::new();
            match self.reader.read_record(&mut record) {
                Ok(true) => self.next_source_row += 1,
                Ok(false) => {
                    self.finished = true;
                    return Ok(None);
                }
                Err(error) => {
                    self.finished = true;
                    return Err(SelectedSourceReadError::MalformedRecord {
                        row: source_row,
                        detail: error.to_string(),
                    });
                }
            }
            if source_row < self.metadata.region.body_start_row {
                continue;
            }
            if let Some((column, field)) = record
                .iter()
                .enumerate()
                .find(|(_, field)| field.len() > self.config.inspection.max_field_size)
            {
                self.finished = true;
                return Err(SelectedSourceReadError::FieldTooLarge {
                    row: source_row,
                    column,
                    size: field.len(),
                    limit: self.config.inspection.max_field_size,
                });
            }
            let fields = record.iter().collect::<Vec<_>>();
            let is_blank = fields
                .iter()
                .all(|field| self.config.normalization.is_blank(field));
            if is_blank {
                self.blank_count += 1;
                if self.blank_count > self.config.candidate_detection.blank_gap_lookahead {
                    self.finished = true;
                    return Ok(None);
                }
                continue;
            }
            let width_compatible = fields.len().abs_diff(width)
                <= self.config.row_classification.max_body_width_difference;
            if width_compatible {
                self.blank_count = 0;
                self.incompatible_count = 0;
            } else {
                self.incompatible_count += 1;
                if self.incompatible_count >= self.config.candidate_detection.footer_lookahead {
                    self.finished = true;
                    return Ok(None);
                }
            }
            let nonblank_count = fields
                .iter()
                .filter(|field| !self.config.normalization.is_blank(field))
                .count();
            let density = if fields.is_empty() {
                0.0
            } else {
                nonblank_count as f64 / fields.len() as f64
            };
            let is_data =
                width_compatible && density >= self.config.row_classification.min_data_density;
            if !is_data {
                continue;
            }
            let cells = (0..width)
                .map(|column| {
                    fields
                        .get(column)
                        .map(|field| SourceCellValue::Present((*field).to_owned()))
                        .unwrap_or(SourceCellValue::Missing)
                })
                .collect();
            return Ok(Some(SelectedRow { source_row, cells }));
        }
    }
}

impl SelectedSourceReader for CsvSelectedSourceReader {
    fn metadata(&self) -> &SelectedSourceMetadata {
        &self.metadata
    }

    fn read_next(
        &mut self,
        limits: RowReadLimits,
    ) -> Result<SelectedRowBatch, SelectedSourceReadError> {
        // Validate directly too: callers can construct limits by literal and
        // bypass RowReadLimits::new.
        if limits.max_rows == 0 || limits.max_bytes == 0 {
            return Err(SelectedSourceReadError::InvalidLimits);
        }
        verify_stamp(&self.path, self.stamp)?;
        let mut rows = Vec::with_capacity(limits.max_rows.min(256));
        let mut bytes = 0usize;
        while rows.len() < limits.max_rows && !self.finished {
            let next = if self.pending.is_some() {
                self.pending.take()
            } else {
                self.read_data_row()?
            };
            let Some(row) = next else {
                break;
            };
            let row_bytes = row
                .cells
                .iter()
                .map(|cell| match cell {
                    SourceCellValue::Present(value) => value.len(),
                    SourceCellValue::Missing => 0,
                })
                .sum::<usize>();
            if row_bytes > limits.max_bytes {
                self.finished = true;
                return Err(SelectedSourceReadError::RowExceedsByteLimit {
                    row: row.source_row,
                    size: row_bytes,
                    limit: limits.max_bytes,
                });
            }
            if bytes.saturating_add(row_bytes) > limits.max_bytes {
                // The row belongs to the next batch. Since the parser has
                // consumed it, retain only this one bounded pending row.
                self.pending = Some(row);
                break;
            }
            bytes += row_bytes;
            rows.push(row);
            if self.next_source_row > self.metadata.region.body_end_row {
                self.finished = true;
            }
        }
        let complete = self.finished && self.pending.is_none();
        verify_stamp(&self.path, self.stamp)?;
        if complete {
            verify_revision(&self.path, &self.metadata.source_revision)?;
        }
        Ok(SelectedRowBatch { rows, complete })
    }
}

fn verify_revision(path: &Path, expected: &SourceRevision) -> Result<(), SelectedSourceReadError> {
    let mut file = File::open(path).map_err(io_error)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual_hash = format!("{:x}", hasher.finalize());
    let actual_size = fs::metadata(path).map_err(io_error)?.len();
    if actual_hash != expected.content_hash || actual_size != expected.file_size {
        return Err(SelectedSourceReadError::RevisionChanged);
    }
    Ok(())
}

fn verify_stamp(path: &Path, expected: FileStamp) -> Result<(), SelectedSourceReadError> {
    let actual = fs::metadata(path).map_err(io_error)?;
    let actual = FileStamp::from(actual);
    if actual.size != expected.size || actual.modified != expected.modified {
        return Err(SelectedSourceReadError::RevisionChanged);
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> SelectedSourceReadError {
    SelectedSourceReadError::Io {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_ingest::SelectedSourceReader;
    use baho_model::candidate::{HeaderCell, HeaderDecision};
    use baho_model::column::ColumnDefinition;
    use baho_model::grid::GridRegion;
    use std::io::Write;

    fn metadata(path: &Path, body_end_row: usize) -> SelectedSourceMetadata {
        let bytes = fs::read(path).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        SelectedSourceMetadata {
            source_revision: SourceRevision {
                content_hash: format!("{:x}", hasher.finalize()),
                file_size: bytes.len() as u64,
                modified_time: None,
            },
            source_sheet_index: 0,
            source_sheet_name: None,
            region: GridRegion {
                id: "region-0".into(),
                header_row: Some(0),
                body_start_row: 1,
                body_end_row,
                col_start: 0,
                col_end: 2,
            },
            header: HeaderDecision {
                source_row: 0,
                cells: (0..2)
                    .map(|col| HeaderCell {
                        col,
                        raw_text: format!("h{col}"),
                        normalized_text: format!("h{col}"),
                        column_id: format!("column-{col}"),
                    })
                    .collect(),
            },
            columns: (0..2)
                .map(|ordinal| ColumnDefinition {
                    id: format!("column-{ordinal}"),
                    ordinal,
                    source_header_raw: None,
                    source_header_normalized: None,
                    display_name: format!("Column {ordinal}"),
                })
                .collect(),
            row_count: baho_ingest::RowCount::Exact(body_end_row),
        }
    }

    #[test]
    fn sequential_batches_keep_quoted_newlines_missing_values_and_coordinates() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"a,b\n\"line one\nline two\",x\ny\nm,\nz,q\n")
            .unwrap();
        let metadata = metadata(file.path(), 4);
        let mut reader =
            CsvSelectedSourceReader::open(file.path(), metadata, ParserConfig::default()).unwrap();

        let first = reader
            .read_next(RowReadLimits::new(1, 100).unwrap())
            .unwrap();
        assert_eq!(first.rows.len(), 1);
        assert_eq!(first.rows[0].source_row, 1);
        assert_eq!(
            first.rows[0].cells[0],
            SourceCellValue::Present("line one\nline two".into())
        );
        assert!(!first.complete);

        let second = reader
            .read_next(RowReadLimits::new(1, 100).unwrap())
            .unwrap();
        assert_eq!(second.rows[0].source_row, 2);
        assert_eq!(second.rows[0].cells[1], SourceCellValue::Missing);

        let third = reader
            .read_next(RowReadLimits::new(1, 100).unwrap())
            .unwrap();
        assert_eq!(third.rows[0].source_row, 3);
        assert_eq!(
            third.rows[0].cells[1],
            SourceCellValue::Present(String::new())
        );
        assert!(!third.complete);

        let fourth = reader
            .read_next(RowReadLimits::new(1, 100).unwrap())
            .unwrap();
        assert_eq!(fourth.rows[0].source_row, 4);
        assert!(fourth.complete);
    }

    #[test]
    fn a_row_that_cannot_fit_the_caller_byte_limit_is_an_error() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"a,b\n12345,x\n").unwrap();
        let mut reader = CsvSelectedSourceReader::open(
            file.path(),
            metadata(file.path(), 1),
            ParserConfig::default(),
        )
        .unwrap();
        assert!(matches!(
            reader.read_next(RowReadLimits::new(2, 3).unwrap()),
            Err(SelectedSourceReadError::RowExceedsByteLimit { row: 1, .. })
        ));
    }

    #[test]
    fn a_changed_source_revision_is_rejected() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"a,b\nx,y\n").unwrap();
        let metadata = metadata(file.path(), 1);
        let mut reader =
            CsvSelectedSourceReader::open(file.path(), metadata, ParserConfig::default()).unwrap();
        file.as_file().set_len(0).unwrap();
        file.write_all(b"a,b\nx,z\n").unwrap();
        assert!(matches!(
            reader.read_next(RowReadLimits::new(1, 100).unwrap()),
            Err(SelectedSourceReadError::RevisionChanged)
        ));
    }

    #[test]
    fn batches_match_selected_region_across_notes_blanks_and_byte_boundaries() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"a,b\n1,x\n,,note\n2,y\n\n3,z\n").unwrap();
        let config = ParserConfig::default();
        let selected = crate::importer::read_selected_region(file.path(), 2, 1, &config).unwrap();
        let mut metadata = metadata(file.path(), selected.body_end_row);
        metadata.row_count = baho_ingest::RowCount::Exact(selected.data_record_count);
        let mut reader = CsvSelectedSourceReader::open(file.path(), metadata, config).unwrap();
        let mut rows = Vec::new();
        loop {
            let batch = reader
                .read_next(RowReadLimits::new(10, 2).unwrap())
                .unwrap();
            assert!(
                batch
                    .rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .map(|cell| match cell {
                        SourceCellValue::Present(value) => value.len(),
                        SourceCellValue::Missing => 0,
                    })
                    .sum::<usize>()
                    <= 2
            );
            rows.extend(batch.rows);
            if batch.complete {
                break;
            }
        }
        assert_eq!(
            rows.iter().map(|row| row.source_row).collect::<Vec<_>>(),
            selected
                .data_records
                .iter()
                .map(|row| row.index)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn oversized_field_reports_source_coordinates() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"a,b\n12345,x\n").unwrap();
        let mut config = ParserConfig::default();
        config.inspection.max_field_size = 4;
        let mut reader =
            CsvSelectedSourceReader::open(file.path(), metadata(file.path(), 1), config).unwrap();
        assert!(matches!(
            reader.read_next(RowReadLimits::new(1, 100).unwrap()),
            Err(SelectedSourceReadError::FieldTooLarge {
                row: 1,
                column: 0,
                size: 5,
                limit: 4
            })
        ));
    }
}
