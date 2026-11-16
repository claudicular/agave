#![cfg(feature = "agave-unstable-api")]
//! Process-global on/off state: a memory-cap trip is re-enabled automatically, an operator's
//! command or a poison is not. (Separate binary: it switches the fast lane on.)

use agave_fast_lane::control;

#[test]
fn test_cap_trip_reenable_rules() {
    control::set_active(true);
    control::disable_for_cap();
    assert!(!control::is_active());
    assert!(control::cap_disabled());
    assert!(control::auto_reenable());
    assert!(control::is_active());
    assert!(!control::cap_disabled());
    assert!(!control::auto_reenable(), "only once per trip");

    // An operator's disable after a trip wins: no automatic re-enable.
    control::disable_for_cap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fast_lane.ctl");
    let tunables = control::Tunables::new(true, true, 0.5, 3);
    let mut last = None;
    std::fs::write(&path, "disable\n").unwrap();
    control::poll_control_file(&path, &mut last, &tunables, false);
    assert!(!control::cap_disabled());
    assert!(!control::auto_reenable());
    assert!(!control::is_active());

    // A poison is never lifted by the cap logic.
    std::fs::write(&path, "enable\n").unwrap();
    control::poll_control_file(&path, &mut last, &tunables, false);
    assert!(control::is_active());
    control::disable_for_cap();
    control::poison("test");
    assert!(!control::auto_reenable());
    assert!(!control::is_active());
}
