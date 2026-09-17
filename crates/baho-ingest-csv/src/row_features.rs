use crate::inspector::LogicalRecord;

/// Per-column shape analysis.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnShape {
    Text,
    Numeric,
    Blank,
    Mixed,
}

/// Computed features for a single row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowFeatures {
    pub index: usize,
    pub physical_width: usize,
    pub nonblank_count: usize,
    pub density: f64,
    pub column_shapes: Vec<ColumnShape>,
    pub normalized_tokens: Vec<String>,
    pub is_blank: bool,
    pub similarity_to_prev: Option<f64>,
}

fn is_blank_value(s: &str) -> bool {
    s.trim().is_empty()
}

fn normalize_token(s: &str) -> String {
    let trimmed = s.trim();
    let mut result = String::with_capacity(trimmed.len());
    let mut prev_was_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                result.push(' ');
            }
            prev_was_space = true;
        } else {
            result.push(ch);
            prev_was_space = false;
        }
    }
    result
}

fn classify_column(values: &[&str]) -> ColumnShape {
    let nonblank: Vec<&str> = values
        .iter()
        .copied()
        .filter(|v| !is_blank_value(v))
        .collect();
    if nonblank.is_empty() {
        return ColumnShape::Blank;
    }
    let all_numeric = nonblank.iter().all(|v| v.trim().parse::<f64>().is_ok());
    let all_text = nonblank.iter().all(|v| v.trim().parse::<f64>().is_err());
    if all_numeric {
        ColumnShape::Numeric
    } else if all_text {
        ColumnShape::Text
    } else {
        ColumnShape::Mixed
    }
}

fn shape_signature(shapes: &[ColumnShape]) -> Vec<u8> {
    shapes
        .iter()
        .map(|s| match s {
            ColumnShape::Text => 0,
            ColumnShape::Numeric => 1,
            ColumnShape::Blank => 2,
            ColumnShape::Mixed => 3,
        })
        .collect()
}

fn similarity(a: &[u8], b: &[u8]) -> f64 {
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 1.0;
    }
    let matching = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
    matching as f64 / max_len as f64
}

/// Compute features for each logical record.
pub fn compute_row_features(records: &[LogicalRecord]) -> Vec<RowFeatures> {
    if records.is_empty() {
        return Vec::new();
    }

    let max_width = records.iter().map(|r| r.fields.len()).max().unwrap_or(0);

    let mut column_values: Vec<Vec<&str>> = vec![Vec::new(); max_width];
    for rec in records {
        for (col, field) in rec.fields.iter().enumerate() {
            column_values[col].push(field.as_str());
        }
    }

    let _column_shapes: Vec<ColumnShape> = column_values
        .iter()
        .map(|vals| classify_column(vals))
        .collect();

    let mut prev_signature: Option<Vec<u8>> = None;

    records
        .iter()
        .map(|rec| {
            let physical_width = rec.fields.len();
            let nonblank_count = rec.fields.iter().filter(|f| !is_blank_value(f)).count();
            let density = if physical_width == 0 {
                0.0
            } else {
                nonblank_count as f64 / physical_width as f64
            };

            let row_shapes: Vec<ColumnShape> = (0..physical_width)
                .map(|col| {
                    let vals: Vec<&str> = vec![rec.fields[col].as_str()];
                    classify_column(&vals)
                })
                .collect();

            let normalized_tokens: Vec<String> =
                rec.fields.iter().map(|f| normalize_token(f)).collect();

            let sig = shape_signature(&row_shapes);
            let similarity_to_prev = prev_signature.as_ref().map(|prev| similarity(prev, &sig));
            prev_signature = Some(sig);

            RowFeatures {
                index: rec.index,
                physical_width,
                nonblank_count,
                density,
                column_shapes: row_shapes,
                normalized_tokens,
                is_blank: rec.is_blank,
                similarity_to_prev,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(index: usize, fields: &[&str]) -> LogicalRecord {
        let fields: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let is_blank = fields.iter().all(|f| is_blank_value(f));
        LogicalRecord {
            index,
            fields,
            is_blank,
        }
    }

    #[test]
    fn column_shape_text() {
        assert_eq!(classify_column(&["hello", "world"]), ColumnShape::Text);
    }

    #[test]
    fn column_shape_numeric() {
        assert_eq!(classify_column(&["42", "3.14"]), ColumnShape::Numeric);
    }

    #[test]
    fn column_shape_blank() {
        assert_eq!(classify_column(&["", "  ", ""]), ColumnShape::Blank);
    }

    #[test]
    fn column_shape_mixed() {
        assert_eq!(classify_column(&["42", "hello"]), ColumnShape::Mixed);
    }

    #[test]
    fn blank_row_detected() {
        let records = vec![make_record(0, &["", "  ", ""])];
        let features = compute_row_features(&records);
        assert!(features[0].is_blank);
        assert_eq!(features[0].nonblank_count, 0);
        assert_eq!(features[0].density, 0.0);
    }

    #[test]
    fn normalized_tokens_collapse_whitespace() {
        let records = vec![make_record(0, &["Floor\nPlan", "  hello   world  "])];
        let features = compute_row_features(&records);
        assert_eq!(features[0].normalized_tokens[0], "Floor Plan");
        assert_eq!(features[0].normalized_tokens[1], "hello world");
    }

    #[test]
    fn density_computation() {
        let records = vec![make_record(0, &["a", "", "c", ""])];
        let features = compute_row_features(&records);
        assert_eq!(features[0].physical_width, 4);
        assert_eq!(features[0].nonblank_count, 2);
        assert!((features[0].density - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn similarity_to_prev_none_for_first_row() {
        let records = vec![make_record(0, &["a", "b"])];
        let features = compute_row_features(&records);
        assert!(features[0].similarity_to_prev.is_none());
    }

    #[test]
    fn similarity_to_prev_computed() {
        let records = vec![make_record(0, &["a", "b"]), make_record(1, &["c", "d"])];
        let features = compute_row_features(&records);
        assert!(features[1].similarity_to_prev.is_some());
        let sim = features[1].similarity_to_prev.unwrap();
        assert!((sim - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_records() {
        let features = compute_row_features(&[]);
        assert!(features.is_empty());
    }
}
