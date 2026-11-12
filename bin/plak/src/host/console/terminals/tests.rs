use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
mod linked;
mod rate_wire;

fn context() -> HostRequestContext {
    HostRequestContext {
        principal: HostPrincipal::Socket(501),
        connection_id: 7,
        host_generation: 9,
        request_id: 1,
    }
}

fn fixture() -> Arc<Console> {
    Console::new(
        crate::home::paths::PlakPaths::explicit("unused".into()),
        "unused".into(),
        None,
        dto::ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
            web_files_read: false,
            web_terminal: false,
        },
    )
}

fn terminal(id: u64) -> dto::TerminalRef {
    dto::TerminalRef {
        id: format!("terminal-{id}"),
        host_generation: 9,
        instance: "instance".into(),
    }
}

fn add(console: &Console, id: u64) -> mpsc::Receiver<Command> {
    let (commands, receiver) = mpsc::channel(8);
    let (resize, _) = watch::channel(None);
    let now = Instant::now();
    let mut state = console.terminals.lock().unwrap();
    state.notifications.entry(owner(&context())).or_default();
    state
        .output
        .entry(context().principal)
        .or_insert_with(|| Budget::new(512 * 1024));
    state.streams.insert(
        id,
        Stream {
            owner: owner(&context()),
            terminal: terminal(id),
            commands,
            resize,
            events: VecDeque::new(),
            seq: 0,
            sent_output: 0,
            next_input: 0,
            acknowledged_input: 0,
            input_tokens: f64::from(65_536),
            input_updated: now,
            ended: false,
            ready: true,
            output: Budget::new(256 * 1024),
        },
    );
    receiver
}

#[test]
fn operator_console_four_stream_credit_rounds_are_fair_bounded_and_atomic() {
    let console = fixture();
    let mut receivers: Vec<_> = (1..=4).map(|id| add(&console, id)).collect();
    for stream in console.terminals.lock().unwrap().streams.values_mut() {
        for offset in (0..WINDOW).step_by(CHUNK) {
            stream.events.push_back(dto::TerminalEvent::Data {
                offset,
                bytes_b64: STANDARD.encode(vec![0xff; CHUNK]),
            });
        }
    }
    for seq in 0..4 {
        for id in 1..=4 {
            let Some(HostFrame::TerminalStream {
                stream,
                seq: actual,
                event: dto::TerminalEvent::Data { offset, bytes_b64 },
                ..
            }) = console.terminal_output(&context())
            else {
                panic!("data")
            };
            assert_eq!((stream, actual, offset), (id, seq, seq * CHUNK as u64));
            assert_eq!(STANDARD.decode(bytes_b64).unwrap(), vec![0xff; CHUNK]);
        }
    }
    assert!(console.terminal_output(&context()).is_none());
    let grants: Vec<_> = (1..=4)
        .map(|stream| dto::TerminalCredit {
            stream,
            bytes: WINDOW,
        })
        .collect();
    let thief = HostRequestContext {
        principal: HostPrincipal::Socket(502),
        ..context()
    };
    assert!(console.terminal_credit(&thief, grants.clone()).is_err());
    let duplicate = vec![grants[0].clone(), grants[0].clone()];
    assert!(console.terminal_credit(&context(), duplicate).is_err());
    assert!(receivers.iter_mut().all(|r| r.try_recv().is_err()));
    assert!(matches!(
        console.terminal_credit(&context(), grants.clone()),
        Ok(HostResponse::TerminalAccepted {})
    ));
    for receiver in &mut receivers {
        assert!(matches!(receiver.try_recv(), Ok(Command::Credit(WINDOW))));
    }
    assert!(console.terminal_credit(&context(), grants).is_err());
    assert!(receivers.iter_mut().all(|r| r.try_recv().is_err()));
    assert!(
        console
            .terminals
            .lock()
            .unwrap()
            .streams
            .values()
            .all(|s| s.sent_output == 0)
    );
}

#[tokio::test]
async fn operator_console_input_refusals_accept_no_bytes_or_offsets_and_never_replay_loss() {
    let console = fixture();
    let mut commands = add(&console, 1);
    let unknown = console.terminal_input(&context(), 99, 0, "YQ==").await;
    assert!(matches!(unknown, Err(dto::ReadRefusal::TerminalLost {})));
    assert!(
        console
            .terminal_input(&context(), 1, 1, "YQ==")
            .await
            .is_err()
    );
    assert!(
        console
            .terminal_input(&context(), 1, 0, "YQ")
            .await
            .is_err()
    );
    assert!(
        console
            .terminal_input(&context(), 1, 0, &STANDARD.encode(vec![0; CHUNK + 1]))
            .await
            .is_err()
    );
    assert!(commands.try_recv().is_err());
    let send = console.clone();
    let pending = tokio::spawn(async move { send.terminal_input(&context(), 1, 0, "AP8=").await });
    let Command::Input {
        offset,
        bytes,
        reply,
    } = commands.recv().await.unwrap()
    else {
        panic!("input")
    };
    assert_eq!((offset, bytes), (0, vec![0, 255]));
    assert!(matches!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            console.terminal_input(&context(), 1, 2, "YQ==")
        )
        .await
        .unwrap(),
        Err(dto::ReadRefusal::TerminalBackpressure {
            retry_after_ms: None
        })
    ));
    assert!(
        commands.try_recv().is_err(),
        "a correct next offset still waits for acknowledgment"
    );
    assert!(
        console
            .terminal_input(&context(), 1, 0, "AP8=")
            .await
            .is_err()
    );
    drop(reply);
    assert!(matches!(
        pending.await.unwrap(),
        Ok(HostResponse::OperationOutcomeUnknown {})
    ));
    assert!(
        console
            .terminal_input(&context(), 1, 0, "AP8=")
            .await
            .is_err()
    );
    assert!(commands.try_recv().is_err());
    assert_eq!(console.terminals.lock().unwrap().streams[&1].next_input, 2);
}

#[tokio::test]
async fn operator_console_small_chunks_are_byte_bounded_and_terminal_end_is_never_dropped() {
    let console = fixture();
    let _commands = add(&console, 1);
    for offset in (0..WINDOW).step_by(256) {
        assert!(console.publish_terminal(
            1,
            dto::TerminalEvent::Data {
                offset,
                bytes_b64: STANDARD.encode(vec![0xff; 256]),
            }
        ));
    }
    assert_eq!(
        console.terminals.lock().unwrap().streams[&1].events.len(),
        4
    );
    assert!(console.publish_terminal(
        1,
        dto::TerminalEvent::Exit {
            status: dto::TerminalStatus::Exited,
            code: Some(0),
            survivors: Vec::new(),
        }
    ));
    assert!(console.terminals.lock().unwrap().streams[&1].ended);
    assert!(matches!(
        console.terminal_input(&context(), 1, 0, "YQ==").await,
        Err(dto::ReadRefusal::TerminalLost {})
    ));
    assert!(matches!(
        console.terminal_credit(
            &context(),
            vec![dto::TerminalCredit {
                stream: 1,
                bytes: 1
            }]
        ),
        Err(dto::ReadRefusal::TerminalLost {})
    ));
    let mut bytes = 0;
    for seq in 0..=4 {
        let Some(HostFrame::TerminalStream {
            seq: actual, event, ..
        }) = console.terminal_output(&context())
        else {
            panic!("missing queued data or terminal end")
        };
        assert_eq!(actual, seq);
        match event {
            dto::TerminalEvent::Data { offset, bytes_b64 } => {
                assert_eq!(offset, bytes);
                bytes += STANDARD.decode(bytes_b64).unwrap().len() as u64;
            }
            dto::TerminalEvent::Exit { .. } => assert_eq!(seq, 4),
            _ => panic!("unexpected event"),
        }
    }
    assert_eq!(bytes, WINDOW);
}

#[test]
fn operator_console_gap_resets_stay_before_their_queued_replay_bytes() {
    let console = fixture();
    let _commands = add(&console, 1);
    for offset in [10, 20, 30] {
        assert!(console.publish_terminal(
            1,
            dto::TerminalEvent::OutputGap {
                oldest: dto::TerminalCursor {
                    instance: "instance".into(),
                    offset
                },
            }
        ));
        if offset != 20 {
            assert!(console.publish_terminal(
                1,
                dto::TerminalEvent::Data {
                    offset,
                    bytes_b64: STANDARD.encode(b"x"),
                }
            ));
        }
    }
    for (seq, offset) in [10, 10, 30, 30].into_iter().enumerate() {
        let Some(HostFrame::TerminalStream {
            seq: actual, event, ..
        }) = console.terminal_output(&context())
        else {
            panic!("missing gap or replay")
        };
        assert_eq!(actual, seq as u64);
        match event {
            dto::TerminalEvent::OutputGap { oldest } => assert_eq!(oldest.offset, offset),
            dto::TerminalEvent::Data { offset: actual, .. } => assert_eq!(actual, offset),
            _ => panic!("unexpected event"),
        }
    }
}

#[test]
fn operator_console_response_precedes_stream_and_disconnect_keeps_no_controller_queue() {
    let console = fixture();
    let mut commands = add(&console, 1);
    {
        let mut state = console.terminals.lock().unwrap();
        let stream = state.streams.get_mut(&1).unwrap();
        stream.ready = false;
        stream.events.push_back(dto::TerminalEvent::Suspended {});
    }
    assert!(console.terminal_output(&context()).is_none());
    let cursor = dto::TerminalCursor {
        instance: "instance".into(),
        offset: 0,
    };
    let response = HostResponse::TerminalAttached {
        terminal: dto::TerminalView {
            terminal: terminal(1),
            session: "session".into(),
            node: "node".into(),
            terminal_id: "terminal".into(),
            status: dto::TerminalStatus::Running,
            oldest: cursor.clone(),
            next: cursor,
            input_next: 0,
            exit_code: None,
            survivors: Vec::new(),
        },
        stream: 1,
        input_credit: WINDOW,
    };
    console.terminal_response_sent(&context(), &response);
    let thief = HostRequestContext {
        connection_id: 8,
        ..context()
    };
    assert!(console.terminal_output(&thief).is_none());
    assert!(matches!(
        console.terminal_output(&context()),
        Some(HostFrame::TerminalStream {
            stream: 1,
            seq: 0,
            event: dto::TerminalEvent::Suspended {},
            ..
        })
    ));
    console.terminal_disconnect(&context());
    assert!(commands.is_closed());
    assert!(commands.try_recv().is_err());
    assert!(console.terminal_notify(&context()).is_none());
    assert!(console.terminals.lock().unwrap().streams.is_empty());
}

#[test]
fn operator_console_terminal_consent_binds_action_principal_connection_and_generation() {
    let console = fixture();
    let action = dto::OperatorAction::TerminalAttach {
        terminal: terminal(1),
        cursor: dto::TerminalCursor {
            instance: "instance".into(),
            offset: 0,
        },
        takeover: true,
    };
    let Ok(HostResponse::OperatorPrepared { consent }) = console.prepare(context(), action.clone())
    else {
        panic!("prepared")
    };
    assert!(consent.summary.contains("not confined"));
    let Ok(HostResponse::OperatorConfirmed { confirmation, .. }) =
        console.confirm(&context(), &consent.nonce)
    else {
        panic!("confirmed")
    };
    for thief in [
        HostRequestContext {
            principal: HostPrincipal::Socket(502),
            ..context()
        },
        HostRequestContext {
            connection_id: 8,
            ..context()
        },
        HostRequestContext {
            host_generation: 10,
            ..context()
        },
    ] {
        assert!(console.consume(&thief, &action, &confirmation).is_err());
    }
    let mut other = action.clone();
    if let dto::OperatorAction::TerminalAttach { takeover, .. } = &mut other {
        *takeover = false;
    }
    assert!(console.consume(&context(), &other, &confirmation).is_err());
    console.consume(&context(), &action, &confirmation).unwrap();
    assert!(console.consume(&context(), &action, &confirmation).is_err());
    assert!(matches!(
        console.terminal_capability(&HostPrincipal::Web("verified".into())),
        Err(RequestRefusal::CapabilityDisabled { .. })
    ));
}

#[test]
fn operator_console_output_rates_do_not_advance_a_blocked_cursor_or_block_another_terminal() {
    let console = fixture();
    let _first = add(&console, 1);
    let _second = add(&console, 2);
    let data = dto::TerminalEvent::Data {
        offset: 0,
        bytes_b64: STANDARD.encode(vec![0; CHUNK]),
    };
    assert!(console.publish_terminal(1, data.clone()));
    assert!(console.publish_terminal(2, data));
    {
        let mut state = console.terminals.lock().unwrap();
        let budget = &mut state.streams.get_mut(&1).unwrap().output;
        budget.tokens = 0.0;
        budget.updated = Instant::now() + std::time::Duration::from_secs(60);
    }
    assert!(matches!(
        console.terminal_output(&context()),
        Some(HostFrame::TerminalStream {
            stream: 2,
            seq: 0,
            ..
        })
    ));
    assert!(console.terminal_output(&context()).is_none());
    {
        let mut state = console.terminals.lock().unwrap();
        let first = state.streams.get_mut(&1).unwrap();
        assert_eq!(
            (first.seq, first.sent_output, first.events.len()),
            (0, 0, 1)
        );
        first.output.updated = Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        let aggregate = state.output.get_mut(&context().principal).unwrap();
        aggregate.tokens = 0.0;
        aggregate.updated = Instant::now() + std::time::Duration::from_secs(60);
    }
    assert!(console.terminal_output(&context()).is_none());
    console
        .terminals
        .lock()
        .unwrap()
        .output
        .get_mut(&context().principal)
        .unwrap()
        .updated = Instant::now()
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(matches!(
        console.terminal_output(&context()),
        Some(HostFrame::TerminalStream {
            stream: 1,
            seq: 0,
            ..
        })
    ));
    assert!(!console.publish_terminal(
        1,
        dto::TerminalEvent::Data {
            offset: CHUNK as u64,
            bytes_b64: STANDARD.encode(vec![0; CHUNK + 1])
        }
    ));
    assert!(
        console.terminals.lock().unwrap().streams[&1]
            .events
            .is_empty()
    );
}
