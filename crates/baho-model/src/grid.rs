use serde::{Deserialize, Serialize};

/// Rectangular bounds of a table candidate within a sheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridRegion {
    /// Stable region identifier.
    pub id: String,
    /// Zero-based row index of the header row, if one was detected.
    pub header_row: Option<usize>,
    /// Zero-based row index of the first body row (inclusive).
    pub body_start_row: usize,
    /// Zero-based row index of the last body row (inclusive).
    pub body_end_row: usize,
    /// Zero-based column index of the leftmost column (inclusive).
    pub col_start: usize,
    /// Zero-based column index of the rightmost column (inclusive).
    pub col_end: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construct_grid_region() {
        let region = GridRegion {
            id: "region-0".to_string(),
            header_row: Some(0),
            body_start_row: 1,
            body_end_row: 10,
            col_start: 0,
            col_end: 3,
        };
        assert_eq!(region.header_row, Some(0));
        assert_eq!(region.body_start_row, 1);
        assert_eq!(region.body_end_row, 10);
        assert_eq!(region.col_start, 0);
        assert_eq!(region.col_end, 3);
    }

    #[test]
    fn header_row_can_be_none() {
        let region = GridRegion {
            id: "r1".to_string(),
            header_row: None,
            body_start_row: 0,
            body_end_row: 5,
            col_start: 0,
            col_end: 2,
        };
        assert!(region.header_row.is_none());
    }

    #[test]
    fn serde_round_trip() {
        let region = GridRegion {
            id: "r2".to_string(),
            header_row: Some(2),
            body_start_row: 3,
            body_end_row: 20,
            col_start: 1,
            col_end: 4,
        };
        let json = serde_json::to_string(&region).unwrap();
        let back: GridRegion = serde_json::from_str(&json).unwrap();
        assert_eq!(region, back);
    }
}
