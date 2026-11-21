use sendspin::audio::{CorrectionPlanner, CorrectionSchedule};

#[test]
fn test_correction_deadband() {
    let planner = CorrectionPlanner::new();
    let schedule = planner.plan(1_000, 48_000, false);
    assert_eq!(schedule, CorrectionSchedule::default());
}

#[test]
fn test_correction_drop() {
    let planner = CorrectionPlanner::new();
    let schedule = planner.plan(200_000, 48_000, false);
    assert!(schedule.drop_every_n_frames > 0);
    assert_eq!(schedule.insert_every_n_frames, 0);
    assert!(!schedule.reanchor);
}

#[test]
fn test_correction_insert() {
    let planner = CorrectionPlanner::new();
    let schedule = planner.plan(-200_000, 48_000, false);
    assert!(schedule.insert_every_n_frames > 0);
    assert_eq!(schedule.drop_every_n_frames, 0);
    assert!(!schedule.reanchor);
}

#[test]
fn test_correction_reanchor() {
    let planner = CorrectionPlanner::new();
    let schedule = planner.plan(600_000, 48_000, false);
    assert!(schedule.reanchor);
}
