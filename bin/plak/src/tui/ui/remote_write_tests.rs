//! A socket-attached terminal cannot silently lose a legacy operator write.

use super::test_support::{blocks_contain, builtin_profiles_view, launch_basic, render_to_string};
use super::{App, ControlTab, SettingsMenu};
use crate::tui::{HostEventReceiver, ProfileApply, SurfaceIntent, UserAction};
use plakd_wire_types::PlakExtension;
use plakous_host_protocol::client::{self, Transport};
use plakous_host_protocol::wire::{Hello, NodeAddress, WIRE_VERSION, encode_line};
use plakous_hosted::host_service::wire::{Face, WireSession, session_welcome};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn serve_remote(host: UnixStream, session: WireSession<PlakExtension>, root: NodeAddress) {
    let mut reader = BufReader::new(host.try_clone().unwrap());
    let mut out = host.try_clone().unwrap();
    let mut hello = String::new();
    reader.read_line(&mut hello).unwrap();
    let welcome = session_welcome::<PlakExtension>("host".into(), 1, "session".into(), root);
    out.write_all(
        encode_line(&plakous_host_protocol::wire::HostFrame::Welcome(welcome))
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    let mut connection = session.connect(host, Face::Remote, || false);
    for line in reader.lines() {
        if !connection.frame(Ok(line.unwrap())).unwrap() {
            break;
        }
        connection.flush().unwrap();
    }
}

#[test]
fn socket_tui_legacy_write_refusals_restore_settings_and_release_refresh() {
    let (host, socket) = UnixStream::pair().unwrap();
    let shutdown = socket.try_clone().unwrap();
    let (actions, intents) = plakous_host_protocol::intent_channel::<PlakExtension>();
    let (_published, events) = plakous_host_protocol::observation_channel::<PlakExtension>();
    let root = NodeAddress {
        tree_id: "session".into(),
        agent_id: "session".into(),
    };
    let session = WireSession::start(root.clone(), "session".into(), actions, events, |_| None);
    let server = std::thread::spawn(move || serve_remote(host, session, root));
    let (sender, receiver, _) = client::connect::<PlakExtension>(
        Transport::new(socket.try_clone().unwrap(), socket),
        Hello {
            proto: WIRE_VERSION,
            binary_id: "terminal-test".into(),
            caps: Vec::new(),
        },
    )
    .unwrap();
    let events = HostEventReceiver::Wire(receiver);
    let mut launch = launch_basic();
    launch.host_mode = true;
    let mut app = App::new(launch, true);
    app.control_tab = Some(ControlTab::Settings);
    let snapshot = crate::config::SettingsView::default();
    app.settings_snapshot = Some(snapshot.clone());
    app.settings_menu = Some(SettingsMenu::open(snapshot.clone()));
    app.settings_menu.as_mut().unwrap().draft.product.compaction = !snapshot.product.compaction;
    app.usage_refresh.active = Some("claude".into());
    let writes = [
        SurfaceIntent::SaveSettings(app.settings_menu.as_ref().unwrap().draft.clone().into()),
        SurfaceIntent::ApplyProfile {
            name: "claude-default".into(),
            apply: ProfileApply::Default,
        },
        SurfaceIntent::RefreshUsageCredential {
            account: "claude".into(),
        },
    ];
    for (write, remedy) in writes.into_iter().zip([
        "plak config set <key> <value>",
        "plak config set root.profile claude-default",
        "plak refresh claude",
    ]) {
        sender.send(UserAction::Extension(write)).unwrap();
        assert!(sender.settle(Duration::from_secs(10)));
        let mut connected = true;
        super::terminal::drain_host_events(&mut app, &events, &mut connected, 64);
        assert!(connected);
        let frame = render_to_string(&mut app, 200, 40);
        assert!(
            frame.contains(remedy),
            "refusal hidden behind Menu: {frame}"
        );
    }
    assert_eq!(
        app.settings_menu.as_ref().unwrap().draft.product,
        snapshot.product
    );
    assert!(app.settings_error.is_some());
    assert!(app.usage_refresh.active.is_none());
    assert!(app.usage_refresh.error.is_some());
    assert!(
        app.next_rearm().is_none(),
        "operator writes are never silent retries"
    );
    for message in [
        "this face may not send that (local_face)",
        "plak config set <key> <value>",
        "plak config set root.profile claude-default",
        "plak refresh claude",
        "plak usage --account claude",
    ] {
        assert!(
            blocks_contain(&app, message),
            "missing visible remedy: {message}"
        );
    }
    while let Ok(intent) = intents.try_recv() {
        assert!(
            !matches!(
                &*intent,
                UserAction::Extension(
                    SurfaceIntent::SaveSettings(_)
                        | SurfaceIntent::ApplyProfile {
                            apply: ProfileApply::Default,
                            ..
                        }
                        | SurfaceIntent::RefreshUsageCredential { .. }
                )
            ),
            "a write reached the session worker"
        );
    }
    shutdown.shutdown(std::net::Shutdown::Both).unwrap();
    server.join().unwrap();
}

#[test]
fn socket_tui_marks_host_writes_read_only_but_keeps_terminal_preferences() {
    let mut launch = launch_basic();
    launch.host_mode = true;
    let mut app = App::new(launch, true);
    app.settings_menu = Some(SettingsMenu::open(crate::config::SettingsView::default()));
    app.settings_snapshot = Some(crate::config::SettingsView::default());
    app.control_tab = Some(ControlTab::Settings);
    let row = crate::tui::settings_registry::ROWS
        .iter()
        .position(|meta| meta.key == "compaction.enabled")
        .unwrap();
    app.settings_menu.as_mut().unwrap().selected = row;
    let before = app.settings_menu.as_ref().unwrap().draft.clone();
    let frame = render_to_string(&mut app, 120, 40);
    assert!(frame.contains("read-only"), "{frame}");
    assert!(frame.contains("config set compaction.enabled"), "{frame}");
    assert!(app.activate_settings_row(row).is_none());
    app.confirm_settings_reset(row);
    assert_eq!(app.settings_menu.as_ref().unwrap().draft, before);
    assert!(
        app.remote_setting_remedy(0).is_none(),
        "theme remains local"
    );
    assert!(
        app.activate_settings_row(1).is_none(),
        "mouse saves locally"
    );
    assert!(!app.preferences.mouse);
    app.apply_profiles_view(builtin_profiles_view());
    app.control_tab = Some(ControlTab::Models);
    app.arm_profile_confirm();
    let frame = render_to_string(&mut app, 120, 40);
    assert!(frame.contains("plak config set root.profile"), "{frame}");
    assert!(!frame.contains("[ Set as default ]"), "{frame}");
    app.destructive_confirm = None;
    app.control_tab = Some(ControlTab::Usage);
    app.usage_refresh.accounts = vec!["claude".into()];
    let frame = render_to_string(&mut app, 120, 40);
    assert!(frame.contains("plak refresh <account>"), "{frame}");
    assert!(!frame.contains("[refresh]"), "{frame}");
    assert!(app.start_usage_refresh("claude".into()).is_none());
    assert!(app.usage_refresh.active.is_none());
}
