use super::*;

#[test]
fn test_path_content_creation() {
    let path = PathContent::new(Rect::new(0.0, 0.0, 100.0, 100.0))
        .with_stroke(Color::black())
        .with_stroke_width(2.0);

    assert!(path.has_stroke());
    assert!(!path.has_fill());
    assert_eq!(path.stroke_width, 2.0);
}

#[test]
fn test_path_from_operations() {
    let ops = vec![
        PathOperation::MoveTo(10.0, 10.0),
        PathOperation::LineTo(50.0, 10.0),
        PathOperation::LineTo(50.0, 50.0),
        PathOperation::LineTo(10.0, 50.0),
        PathOperation::ClosePath,
    ];

    let path = PathContent::from_operations(ops);

    assert_eq!(path.bbox.x, 10.0);
    assert_eq!(path.bbox.y, 10.0);
    assert_eq!(path.bbox.width, 40.0);
    assert_eq!(path.bbox.height, 40.0);
}

#[test]
fn test_path_with_fill() {
    let path =
        PathContent::new(Rect::new(0.0, 0.0, 100.0, 100.0)).with_fill(Color::new(1.0, 0.0, 0.0));

    assert!(path.has_fill());
    assert!(path.has_stroke()); // Default has stroke
}

#[test]
fn test_compute_bbox_from_rectangle() {
    let ops = vec![PathOperation::Rectangle(20.0, 30.0, 100.0, 50.0)];
    let path = PathContent::from_operations(ops);

    assert_eq!(path.bbox.x, 20.0);
    assert_eq!(path.bbox.y, 30.0);
    assert_eq!(path.bbox.width, 100.0);
    assert_eq!(path.bbox.height, 50.0);
}

// === to_points (issue #147) ===

/// Ground-truth cubic Bézier evaluation, used to validate flattening.
fn cubic_at(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32), t: f32) -> (f32, f32) {
    let mt = 1.0 - t;
    let x =
        mt * mt * mt * p0.0 + 3.0 * mt * mt * t * p1.0 + 3.0 * mt * t * t * p2.0 + t * t * t * p3.0;
    let y =
        mt * mt * mt * p0.1 + 3.0 * mt * mt * t * p1.1 + 3.0 * mt * t * t * p2.1 + t * t * t * p3.1;
    (x, y)
}

/// Shortest distance from a point to a polyline (min over its segments),
/// reusing the production segment-distance helper.
fn dist_to_polyline(p: (f32, f32), poly: &[(f32, f32)]) -> f32 {
    poly.windows(2)
        .map(|s| dist_point_to_segment(p, s[0], s[1]))
        .fold(f32::MAX, f32::min)
}

#[test]
fn test_to_points_straight_line_passthrough() {
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(10.0, 10.0),
        PathOperation::LineTo(100.0, 40.0),
    ]);
    let pts = path.to_points(0.5);
    assert_eq!(pts, vec![vec![(10.0, 10.0), (100.0, 40.0)]]);
}

#[test]
fn test_to_points_curve_endpoints_preserved() {
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(0.0, 0.0),
        PathOperation::CurveTo(0.0, 100.0, 100.0, 100.0, 100.0, 0.0),
    ]);
    let sub = &path.to_points(0.1)[0];
    assert_eq!(sub.first().copied(), Some((0.0, 0.0)), "must start at P0");
    let last = sub.last().copied().unwrap();
    assert!(
        (last.0 - 100.0).abs() < 1e-3 && (last.1 - 0.0).abs() < 1e-3,
        "must end at P3, got {last:?}"
    );
}

#[test]
fn test_to_points_curve_within_tolerance() {
    let p0 = (0.0, 0.0);
    let p1 = (0.0, 100.0);
    let p2 = (100.0, 100.0);
    let p3 = (100.0, 0.0);
    let tol = 0.5;
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(p0.0, p0.1),
        PathOperation::CurveTo(p1.0, p1.1, p2.0, p2.1, p3.0, p3.1),
    ]);
    let poly = &path.to_points(tol)[0];
    // Every point on the true curve must lie within `tol` of the polyline.
    for i in 0..=200 {
        let t = i as f32 / 200.0;
        let truth = cubic_at(p0, p1, p2, p3, t);
        let d = dist_to_polyline(truth, poly);
        assert!(
            d <= tol + 1e-3,
            "curve point at t={t} is {d} from polyline (tol={tol})"
        );
    }
}

#[test]
fn test_to_points_tolerance_monotonic() {
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(0.0, 0.0),
        PathOperation::CurveTo(0.0, 100.0, 100.0, 100.0, 100.0, 0.0),
    ]);
    let coarse = path.to_points(10.0)[0].len();
    let fine = path.to_points(0.05)[0].len();
    assert!(
        fine >= coarse,
        "finer tolerance must not reduce point count ({fine} < {coarse})"
    );
    assert!(
        fine > 2,
        "fine flattening must densify the curve, got {fine}"
    );
}

#[test]
fn test_to_points_rectangle_is_closed_subpath() {
    let path = PathContent::from_operations(vec![PathOperation::Rectangle(10.0, 20.0, 30.0, 40.0)]);
    let pts = path.to_points(1.0);
    assert_eq!(
        pts,
        vec![vec![
            (10.0, 20.0),
            (40.0, 20.0),
            (40.0, 60.0),
            (10.0, 60.0),
            (10.0, 20.0)
        ]]
    );
}

#[test]
fn test_to_points_segment_after_rectangle_continues_from_current_point() {
    // Per PDF §8.5.2 the current point after `re` is the rectangle's
    // lower-left; a following segment continues from there as a new subpath.
    let path = PathContent::from_operations(vec![
        PathOperation::Rectangle(0.0, 0.0, 10.0, 10.0),
        PathOperation::LineTo(20.0, 20.0),
    ]);
    let pts = path.to_points(1.0);
    assert_eq!(pts.len(), 2);
    assert_eq!(
        pts[0],
        vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0)
        ]
    );
    assert_eq!(pts[1], vec![(0.0, 0.0), (20.0, 20.0)]);
}

#[test]
fn test_to_points_closepath_appends_start() {
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(0.0, 0.0),
        PathOperation::LineTo(10.0, 0.0),
        PathOperation::LineTo(10.0, 10.0),
        PathOperation::ClosePath,
    ]);
    assert_eq!(
        path.to_points(1.0),
        vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 0.0)]]
    );
}

#[test]
fn test_to_points_closepath_terminates_subpath() {
    // Per §8.5.2 Table 59, `h` terminates the subpath; a following segment
    // begins a NEW subpath (seeded from the subpath start = current point),
    // rather than extending the closed loop.
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(0.0, 0.0),
        PathOperation::LineTo(10.0, 0.0),
        PathOperation::ClosePath,
        PathOperation::LineTo(5.0, 5.0),
    ]);
    let pts = path.to_points(1.0);
    assert_eq!(pts.len(), 2, "h must terminate the subpath");
    assert_eq!(pts[0], vec![(0.0, 0.0), (10.0, 0.0), (0.0, 0.0)]);
    assert_eq!(pts[1], vec![(0.0, 0.0), (5.0, 5.0)]);
}

#[test]
fn test_to_points_consecutive_moveto_leaves_no_vestige() {
    // Per §8.5.2 Table 59, a consecutive `m` overrides the previous one with
    // no vestige; the orphaned start point must not appear as a subpath.
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(1.0, 1.0),
        PathOperation::MoveTo(2.0, 2.0),
        PathOperation::LineTo(3.0, 3.0),
    ]);
    assert_eq!(path.to_points(1.0), vec![vec![(2.0, 2.0), (3.0, 3.0)]]);
}

#[test]
fn test_to_points_lone_moveto_is_dropped() {
    // A subpath that adds no segment paints nothing and yields no polyline.
    let path = PathContent::from_operations(vec![PathOperation::MoveTo(5.0, 5.0)]);
    assert!(path.to_points(1.0).is_empty());
}

#[test]
fn test_to_points_multiple_subpaths() {
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(0.0, 0.0),
        PathOperation::LineTo(1.0, 0.0),
        PathOperation::MoveTo(5.0, 5.0),
        PathOperation::LineTo(6.0, 5.0),
    ]);
    assert_eq!(
        path.to_points(1.0),
        vec![vec![(0.0, 0.0), (1.0, 0.0)], vec![(5.0, 5.0), (6.0, 5.0)]]
    );
}

#[test]
fn test_to_points_empty() {
    let path = PathContent::from_operations(vec![]);
    assert!(path.to_points(1.0).is_empty());
}

#[test]
fn test_to_points_nonpositive_tolerance_terminates() {
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(0.0, 0.0),
        PathOperation::CurveTo(0.0, 100.0, 100.0, 100.0, 100.0, 0.0),
    ]);
    for tol in [0.0, -1.0, f32::NAN] {
        let pts = path.to_points(tol);
        assert_eq!(pts.len(), 1);
        let n = pts[0].len();
        assert!(
            (2..100_000).contains(&n),
            "tol={tol} produced {n} points (expected bounded)"
        );
        assert!(pts[0].iter().all(|(x, y)| x.is_finite() && y.is_finite()));
    }
}

#[test]
fn test_to_points_curve_starts_at_current_point() {
    // The curve's implicit P0 is the current point (here the LineTo endpoint),
    // not the path origin. Guards against using the wrong start point.
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(0.0, 0.0),
        PathOperation::LineTo(50.0, 0.0),
        PathOperation::CurveTo(60.0, 20.0, 70.0, 20.0, 80.0, 0.0),
    ]);
    let sub = &path.to_points(0.5)[0];
    assert_eq!(sub[0], (0.0, 0.0));
    assert_eq!(sub[1], (50.0, 0.0));
    // First flattened curve vertex departs from the LineTo endpoint (50,0),
    // so it must sit to its right (x > 50), never back near the origin.
    assert!(
        sub[2].0 > 50.0,
        "curve did not start at current point; sub[2]={:?}",
        sub[2]
    );
    assert!((sub.last().unwrap().0 - 80.0).abs() < 1e-3);
}

#[test]
fn test_to_points_degenerate_curve() {
    // All control points coincide: a zero-length curve must not subdivide forever.
    let path = PathContent::from_operations(vec![
        PathOperation::MoveTo(5.0, 5.0),
        PathOperation::CurveTo(5.0, 5.0, 5.0, 5.0, 5.0, 5.0),
    ]);
    let sub = &path.to_points(0.1)[0];
    assert!(
        sub.len() < 10,
        "degenerate curve over-subdivided: {} points",
        sub.len()
    );
    assert!(sub.iter().all(|&p| p == (5.0, 5.0)));
}
