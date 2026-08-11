use super::*;

/// 6 columns, 6 rows, modal rows alternate between {0,1,2} and
/// {3,4,5}. Two disconnected components of 3 columns each, each
/// with 3 modal rows of support. Default profile.
#[test]
fn validate_rejects_split_column_groups() {
    let grid = make_split_grid(6, 6);
    let config = TableDetectionConfig::default();
    assert!(!validate_table_structure_internal(&grid, &config));
}

/// Same fixture, strict profile. The strict profile's stronger
/// regular_row_ratio (0.8) does not catch this; the split-column
/// detector does.
#[test]
fn validate_rejects_split_column_groups_under_strict_profile() {
    let grid = make_split_grid(6, 6);
    let config = TableDetectionConfig::strict();
    assert!(!validate_table_structure_internal(&grid, &config));
}

/// 11 columns, 5 rows, every row populates every column. One
/// component spanning all columns → accepted.
#[test]
fn validate_accepts_dense_table() {
    let grid = make_uniform_grid(11, 5, 11);
    let config = TableDetectionConfig::default();
    assert!(validate_table_structure_internal(&grid, &config));
}

/// 6 columns, 5 rows, every row populates the first 4 columns.
/// The old density gate would have admitted this at the boundary
/// (4/6 = 2/3). One connected component of 4 columns → accepted.
#[test]
fn validate_accepts_sparse_connected_table() {
    let grid = make_uniform_grid(6, 5, 4);
    let config = TableDetectionConfig::default();
    assert!(validate_table_structure_internal(&grid, &config));
}

/// 8 columns, 12 rows, grouped row-headers occupy the first 2
/// columns and are populated only in the first row of each group
/// of 4. Models arxiv_2510.24670v2's failure shape (post-
/// clustering): 9 modal data rows populate columns 2..8, six data
/// columns all connected, one component → accepted. The real
/// failure may also involve upstream column over-counting; this
/// fixture pins the validator-level property we care about:
/// sparse modal rows whose populated columns form one connected
/// component must be accepted.
#[test]
fn validate_accepts_hierarchical_grouped_table() {
    let grid = make_grouped_grid(8, 12, 2, 4);
    let config = TableDetectionConfig::default();
    assert!(validate_table_structure_internal(&grid, &config));
}

/// num_cols = 3 short-circuits has_split_modal_column_groups
/// (num_cols < 4), so a small dense grid passes. Documents the
/// boundary.
#[test]
fn validate_accepts_three_column_grid() {
    let grid = make_uniform_grid(3, 4, 3);
    let config = TableDetectionConfig::default();
    assert!(validate_table_structure_internal(&grid, &config));
}
