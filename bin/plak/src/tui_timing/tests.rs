use super::*;

#[test]
fn late_writer_registration_flushes_launch_phases_without_another_phase()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let paths = PlakPaths::from_home(directory.path().to_path_buf());
    attach(&paths, "00000000-0000-0000-0000-000000000001", false);
    record(
        StartupTimingPhase::ReadyToTypeLocal,
        Duration::from_millis(20),
    );
    let (writer, receiver) = crate::telemetry_sink::TelemetryWriter::detached(8);
    crate::telemetry_sink::register_telemetry_writer(paths.home(), &writer);
    assert_eq!(receiver.try_iter().count(), 2);
    Ok(())
}

#[test]
fn launch_phases_wait_for_the_background_telemetry_writer() {
    let paths = PlakPaths::from_home(std::env::temp_dir());
    let session_id = session_id_from_text("00000000-0000-0000-0000-000000000001")
        .unwrap_or_else(|| unreachable!("fixed valid session"));
    let mut timing = Timing {
        paths,
        session_id,
        resumed: false,
        first_frame: false,
        host_ready: false,
        quit_requested: None,
        restored: None,
        pending: Vec::new(),
        exit_import_due: false,
    };
    timing.record(None, StartupTimingPhase::ProcessStart, Duration::ZERO);
    timing.record(
        None,
        StartupTimingPhase::FirstFrameLocal,
        Duration::from_millis(20),
    );
    timing.record(
        None,
        StartupTimingPhase::ReadyToTypeLocal,
        Duration::from_millis(20),
    );
    let (writer, receiver) = crate::telemetry_sink::TelemetryWriter::detached(8);
    timing.record(
        Some(&writer),
        StartupTimingPhase::HostReadyBackground,
        Duration::from_secs(2),
    );
    let phases: Vec<_> =
        receiver
            .try_iter()
            .filter_map(|item| match item {
                crate::telemetry_store::TelemetryItem::Product(
                    ProductObservation::StartupTiming { phase, elapsed, .. },
                ) => Some((phase, elapsed)),
                _ => None,
            })
            .collect();
    assert_eq!(
        phases,
        vec![
            (StartupTimingPhase::ProcessStart, Duration::ZERO),
            (
                StartupTimingPhase::FirstFrameLocal,
                Duration::from_millis(20)
            ),
            (
                StartupTimingPhase::ReadyToTypeLocal,
                Duration::from_millis(20)
            ),
            (
                StartupTimingPhase::HostReadyBackground,
                Duration::from_secs(2)
            ),
        ]
    );
    assert_eq!(timing.pending, [] as [ProductObservation; 0]);
}

#[test]
fn an_exit_imported_before_writer_registration_is_emitted_on_registration() {
    let session = "00000000-0000-0000-0000-000000000001";
    let mut timing = Timing {
        paths: PlakPaths::from_home(std::env::temp_dir()),
        session_id: session_id_from_text(session)
            .unwrap_or_else(|| unreachable!("fixed valid session")),
        resumed: true,
        first_frame: true,
        host_ready: false,
        quit_requested: None,
        restored: None,
        pending: Vec::new(),
        exit_import_due: false,
    };
    let observations = exit_observations(vec![ExitMeasurement {
        session: session.to_owned(),
        resumed: false,
        key_to_restore_ns: 4_000_000,
        restore_to_exit_ns: 12_000_000,
    }]);
    timing.emit(None, observations);
    let (writer, receiver) = crate::telemetry_sink::TelemetryWriter::detached(8);
    timing.flush_pending(&writer);
    let phases: Vec<_> =
        receiver
            .try_iter()
            .filter_map(|item| match item {
                crate::telemetry_store::TelemetryItem::Product(
                    ProductObservation::StartupTiming { phase, elapsed, .. },
                ) => Some((phase, elapsed)),
                _ => None,
            })
            .collect();
    assert_eq!(
        phases,
        vec![
            (StartupTimingPhase::KeyToRestore, Duration::from_millis(4)),
            (StartupTimingPhase::RestoreToExit, Duration::from_millis(12)),
        ]
    );
}

#[test]
fn exit_measurement_survives_until_the_next_open() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let paths = PlakPaths::from_home(directory.path().to_path_buf());
    fs::create_dir_all(directory.path().join("logs"))?;
    let measurement = ExitMeasurement {
        session: "00000000-0000-0000-0000-000000000001".to_owned(),
        resumed: true,
        key_to_restore_ns: 4_000_000,
        restore_to_exit_ns: 12_000_000,
    };
    write_exit_measurement(&paths, &measurement)?;
    let imported = take_exit_measurements(&paths);
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].restore_to_exit_ns, 12_000_000);
    assert!(take_exit_measurements(&paths).is_empty());
    Ok(())
}
