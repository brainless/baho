use baho_model::candidate::{RowClassification, RowKind};

use crate::candidates::CandidateConfig;
use crate::row_features::RowFeatures;

/// Classify body rows after a header row.
pub fn classify_rows(
    features: &[RowFeatures],
    header_row: usize,
    config: &CandidateConfig,
) -> Vec<RowClassification> {
    let mut classifications = Vec::new();

    let header_width = features
        .get(header_row)
        .map(|f| f.physical_width)
        .unwrap_or(0);

    let start = header_row + 1;
    if start >= features.len() {
        return classifications;
    }

    let mut consecutive_footer = 0usize;

    for i in start..features.len() {
        let feat = &features[i];

        if feat.is_blank {
            consecutive_footer = 0;
            classifications.push(RowClassification {
                source_row: i,
                kind: RowKind::BlankSeparator,
                reason: Some("all fields blank".to_string()),
            });
            continue;
        }

        let width_diff = if feat.physical_width > header_width {
            feat.physical_width - header_width
        } else {
            header_width - feat.physical_width
        };

        let density_drop = feat.density < 0.3;
        let width_incompatible = width_diff > 2;

        if width_incompatible || density_drop {
            consecutive_footer += 1;
            let kind = if consecutive_footer >= config.footer_lookahead {
                RowKind::Footer
            } else {
                RowKind::Note
            };
            let reason = if width_incompatible {
                Some(format!(
                    "width {} vs header width {}",
                    feat.physical_width, header_width
                ))
            } else {
                Some(format!("low density {:.2}", feat.density))
            };
            classifications.push(RowClassification {
                source_row: i,
                kind,
                reason,
            });
        } else {
            consecutive_footer = 0;
            classifications.push(RowClassification {
                source_row: i,
                kind: RowKind::Data,
                reason: None,
            });
        }
    }

    promote_notes_to_footer(&mut classifications, config);
    classifications
}

fn promote_notes_to_footer(classifications: &mut [RowClassification], config: &CandidateConfig) {
    let mut note_run_start: Option<usize> = None;
    let mut note_run_len = 0usize;

    for idx in 0..classifications.len() {
        if classifications[idx].kind == RowKind::Note {
            if note_run_start.is_none() {
                note_run_start = Some(idx);
                note_run_len = 1;
            } else {
                note_run_len += 1;
            }
        } else {
            if note_run_len >= config.footer_lookahead {
                if let Some(start) = note_run_start {
                    for item in classifications[start..start + note_run_len].iter_mut() {
                        item.kind = RowKind::Footer;
                    }
                }
            }
            note_run_start = None;
            note_run_len = 0;
        }
    }

    if note_run_len >= config.footer_lookahead {
        if let Some(start) = note_run_start {
            for item in classifications[start..start + note_run_len].iter_mut() {
                item.kind = RowKind::Footer;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_features(index: usize, width: usize, nonblank: usize, is_blank: bool) -> RowFeatures {
        let density = if width == 0 {
            0.0
        } else {
            nonblank as f64 / width as f64
        };
        RowFeatures {
            index,
            physical_width: width,
            nonblank_count: nonblank,
            density,
            column_shapes: vec![],
            normalized_tokens: vec![],
            is_blank,
            similarity_to_prev: None,
        }
    }

    #[test]
    fn data_rows_with_consistent_width() {
        let features = vec![
            make_features(0, 3, 3, false),
            make_features(1, 3, 3, false),
            make_features(2, 3, 2, false),
            make_features(3, 3, 3, false),
        ];
        let config = CandidateConfig::default();
        let cls = classify_rows(&features, 0, &config);
        assert_eq!(cls.len(), 3);
        assert!(cls.iter().all(|c| c.kind == RowKind::Data));
    }

    #[test]
    fn blank_rows_classified() {
        let features = vec![
            make_features(0, 3, 3, false),
            make_features(1, 3, 3, false),
            make_features(2, 0, 0, true),
            make_features(3, 3, 3, false),
        ];
        let config = CandidateConfig::default();
        let cls = classify_rows(&features, 0, &config);
        assert_eq!(cls[1].kind, RowKind::BlankSeparator);
        assert_eq!(cls[0].kind, RowKind::Data);
        assert_eq!(cls[2].kind, RowKind::Data);
    }

    #[test]
    fn footer_after_sustained_width_change() {
        let mut features = vec![make_features(0, 3, 3, false)];
        for i in 1..=3 {
            features.push(make_features(i, 3, 3, false));
        }
        for i in 4..14 {
            features.push(make_features(i, 6, 2, false));
        }
        let config = CandidateConfig::default();
        let cls = classify_rows(&features, 0, &config);
        let footer_count = cls.iter().filter(|c| c.kind == RowKind::Footer).count();
        assert!(footer_count > 0);
    }

    #[test]
    fn isolated_note_row() {
        let features = vec![
            make_features(0, 3, 3, false),
            make_features(1, 3, 3, false),
            make_features(2, 6, 1, false),
            make_features(3, 3, 3, false),
        ];
        let config = CandidateConfig::default();
        let cls = classify_rows(&features, 0, &config);
        assert_eq!(cls[1].kind, RowKind::Note);
        assert_eq!(cls[0].kind, RowKind::Data);
        assert_eq!(cls[2].kind, RowKind::Data);
    }
}
