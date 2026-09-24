use super::*;
use crossterm::event::KeyEventState;

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[test]
fn ctrl_t_enters_command_mode_without_sending() {
    let (state, action) =
        EscapeState::Normal.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));

    assert_eq!(state, EscapeState::AwaitingCommand);
    assert_eq!(action, InputAction::Ignore);
}

#[test]
fn ctrl_t_q_quits() {
    let (state, action) =
        EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE));

    assert_eq!(state, EscapeState::Normal);
    assert_eq!(action, InputAction::Command(SessionCommand::Quit));
}

#[test]
fn ctrl_t_core_commands_dispatch_to_their_commands() {
    for (character, modifiers, command) in [
        ('c', KeyModifiers::NONE, SessionCommand::ShowConfig),
        ('l', KeyModifiers::NONE, SessionCommand::ClearScreen),
        ('t', KeyModifiers::NONE, SessionCommand::ToggleTimestamps),
        ('d', KeyModifiers::NONE, SessionCommand::CycleDownCore),
        ('?', KeyModifiers::SHIFT, SessionCommand::Help),
    ] {
        assert_eq!(
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char(character), modifiers)),
            (EscapeState::Normal, InputAction::Command(command))
        );
    }

    assert_eq!(
        EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('R'), KeyModifiers::SHIFT)),
        (
            EscapeState::Normal,
            InputAction::Command(SessionCommand::ResetTarget)
        )
    );
}

#[test]
fn next_routable_wraps_around_in_order() {
    assert_eq!(
        next_routable(&[CoreId::new(0), CoreId::new(1)], CoreId::new(0)),
        CoreId::new(1)
    );
    assert_eq!(
        next_routable(&[CoreId::new(0), CoreId::new(1)], CoreId::new(1)),
        CoreId::new(0)
    );
    assert_eq!(
        next_routable(&[CoreId::new(1)], CoreId::new(1)),
        CoreId::new(1)
    );
    assert_eq!(
        next_routable(&[CoreId::new(0), CoreId::new(2)], CoreId::new(0)),
        CoreId::new(2)
    );
}

#[test]
fn cycling_requires_another_routable_core() {
    let core = CoreId::new(0);
    let other = CoreId::new(1);
    let mut routes = DownRoutes::new(&[]);
    assert_eq!(routes.cycle(), None);
    assert_eq!(routes.target(), None);

    routes.set_routable(&[core]);
    assert_eq!(routes.cycle(), None);
    assert_eq!(routes.target(), Some(core));

    routes.set_routable(&[core, other]);
    assert_eq!(routes.cycle(), Some(other));
    assert_eq!(routes.cycle(), Some(core));
}

#[test]
fn first_down_route_is_selected_and_remembered_across_outage() {
    let first = CoreId::new(2);
    let alternate = CoreId::new(3);
    let mut routes = DownRoutes::new(&[]);
    assert_eq!(routes.active_target(), None);

    routes.set_routable(&[first, alternate]);
    assert_eq!(routes.active_target(), Some(first));
    routes.set_routable(&[]);
    assert_eq!(routes.active_target(), None);
    assert_eq!(routes.target(), Some(first));
    assert_eq!(routes.queue(b"unavailable"), Err(NoRoutableDownChannel));

    routes.set_routable(&[alternate, first]);
    assert_eq!(routes.active_target(), Some(first));
    routes.set_routable(&[alternate]);
    assert_eq!(routes.active_target(), Some(alternate));
}

#[test]
fn down_routes_keep_queued_bytes_on_their_original_core() {
    let mut routes = DownRoutes::new(&[CoreId::new(0), CoreId::new(1)]);
    routes.queue(b"for-core-0").unwrap();
    routes.cycle();
    assert_eq!(routes.target(), Some(CoreId::new(1)));
    routes.queue(b"for-core-1").unwrap();

    let mut sent = Vec::new();
    routes
        .flush_pending(|id, bytes| {
            sent.push((id, bytes.to_vec()));
            Ok(bytes.len())
        })
        .unwrap();
    // Cycling only retargets; the new core's prompt comes from the
    // renderer's cache, so no extra bytes are queued for it.
    assert_eq!(
        sent,
        vec![
            (CoreId::new(0), b"for-core-0".to_vec()),
            (CoreId::new(1), b"for-core-1".to_vec()),
        ]
    );

    let mut replayed = Vec::new();
    routes
        .flush_pending(|id, bytes| {
            replayed.push((id, bytes.to_vec()));
            Ok(bytes.len())
        })
        .unwrap();
    assert!(replayed.is_empty());
}

#[test]
fn down_routes_partial_writes_stay_on_their_route() {
    let mut routes = DownRoutes::new(&[CoreId::new(0), CoreId::new(1)]);
    routes.queue(b"abcdef").unwrap();
    routes
        .flush_pending(|id, bytes| {
            if id == CoreId::new(0) {
                Ok(bytes.len().min(2))
            } else {
                Ok(bytes.len())
            }
        })
        .unwrap();

    routes.cycle();
    let mut core0_rest = Vec::new();
    routes
        .flush_pending(|id, bytes| {
            if id == CoreId::new(0) {
                core0_rest.extend_from_slice(bytes);
            }
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(core0_rest, b"cdef");
}

#[test]
fn down_routes_discard_lost_cores_pending_suffix_without_replaying_on_reconnect() {
    let mut routes = DownRoutes::new(&[CoreId::new(0), CoreId::new(1)]);
    routes.cycle();
    routes.queue(b"unfinished").unwrap();
    routes.set_routable(&[CoreId::new(0)]);
    assert_eq!(routes.clear_core(CoreId::new(1)), b"unfinished".len());
    assert_eq!(routes.clear_core(CoreId::new(1)), 0);
    routes.queue(b"healthy").unwrap();
    let mut sent = Vec::new();
    routes
        .flush_pending(|id, bytes| {
            sent.push((id, bytes.to_vec()));
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(sent, vec![(CoreId::new(0), b"healthy".to_vec())]);

    routes.set_routable(&[]);
    routes.set_routable(&[CoreId::new(1)]);
    routes
        .flush_pending(|id, bytes| {
            sent.push((id, bytes.to_vec()));
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(sent.len(), 1);
}

#[test]
fn down_routes_clear_only_reset_cores_pending_input() {
    let mut routes = DownRoutes::new(&[CoreId::new(0), CoreId::new(1)]);
    routes.queue(b"stale").unwrap();
    routes.cycle();
    routes.queue(b"live").unwrap();
    assert_eq!(routes.clear_core(CoreId::new(0)), b"stale".len());
    let mut sent = Vec::new();
    routes
        .flush_pending(|id, bytes| {
            sent.push((id, bytes.to_vec()));
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(sent, vec![(CoreId::new(1), b"live".to_vec())]);
}

#[test]
fn full_ring_zero_write_keeps_suffix_until_later_write() {
    let core = CoreId::new(0);
    let mut routes = DownRoutes::new(&[core]);
    routes.queue(b"command").unwrap();
    routes.flush_pending(|_, _| Ok(0)).unwrap();
    let mut sent = Vec::new();
    routes
        .flush_pending(|_, bytes| {
            sent.extend_from_slice(bytes);
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(sent, b"command");
}

#[test]
fn unavailable_route_drops_new_keyboard_bytes_but_keeps_command_mode() {
    let core = CoreId::new(0);
    let mut routes = DownRoutes::new(&[]);
    assert_eq!(routes.queue(b"stale"), Err(NoRoutableDownChannel));
    let (state, action) =
        EscapeState::Normal.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert_eq!(action, InputAction::Ignore);
    assert_eq!(state, EscapeState::AwaitingCommand);
    let (_, action) = state.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE));
    assert_eq!(action, InputAction::Command(SessionCommand::Quit));
    let (_, action) = state.handle_key(key(KeyCode::Char('R'), KeyModifiers::NONE));
    assert_eq!(action, InputAction::Command(SessionCommand::ResetTarget));

    routes.set_routable(&[core]);
    let mut sent = false;
    routes
        .flush_pending(|_, _| {
            sent = true;
            Ok(0)
        })
        .unwrap();
    assert!(!sent);
    assert_eq!(routes.queue(b"fresh"), Ok(()));
    routes
        .flush_pending(|_, bytes| {
            assert_eq!(bytes, b"fresh");
            Ok(bytes.len())
        })
        .unwrap();
}

#[test]
fn down_routing_errors_when_explicit_channel_is_proven_absent() {
    let error = DownRouting::new()
        .reconcile(
            &[],
            &[],
            true,
            Some(ChannelId::from_cli(7, "test").unwrap()),
            true,
        )
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "down channel 7 does not exist on any configured core"
    );
}

#[test]
fn ctrl_t_ctrl_t_sends_literal_ctrl_t() {
    let (state, action) =
        EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));

    assert_eq!(state, EscapeState::Normal);
    assert_eq!(action, InputAction::Send(vec![0x14]));
}

#[test]
fn ctrl_c_is_forwarded_to_the_target() {
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        (EscapeState::Normal, InputAction::Send(vec![0x03]))
    );
}

#[test]
fn ordinary_keys_keep_existing_encoding() {
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
    );
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
        (EscapeState::Normal, InputAction::Send(vec![b'\n']))
    );
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE)),
        (EscapeState::Normal, InputAction::Send(vec![0x7f]))
    );
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Up, KeyModifiers::NONE)),
        (EscapeState::Normal, InputAction::Send(b"\x1b[A".to_vec()))
    );
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Down, KeyModifiers::NONE)),
        (EscapeState::Normal, InputAction::Send(b"\x1b[B".to_vec()))
    );
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Left, KeyModifiers::NONE)),
        (EscapeState::Normal, InputAction::Send(b"\x1b[D".to_vec()))
    );
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Right, KeyModifiers::NONE)),
        (EscapeState::Normal, InputAction::Send(b"\x1b[C".to_vec()))
    );
}

#[test]
fn control_and_alt_keys_keep_terminal_encoding() {
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        (EscapeState::Normal, InputAction::Send(vec![1]))
    );
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        (EscapeState::Normal, InputAction::Send(vec![21]))
    );
    assert_eq!(
        EscapeState::Normal.handle_key(key(KeyCode::Char('b'), KeyModifiers::ALT)),
        (EscapeState::Normal, InputAction::Send(b"\x1bb".to_vec()))
    );
}

#[test]
fn repeated_keys_are_forwarded() {
    let mut repeated = key(KeyCode::Char('x'), KeyModifiers::NONE);
    repeated.kind = KeyEventKind::Repeat;

    assert_eq!(
        EscapeState::Normal.handle_key(repeated),
        (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
    );
}

#[test]
fn unknown_command_keys_pass_through_and_reset_state() {
    assert_eq!(
        EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
    );
}

#[test]
fn key_releases_do_not_change_escape_state() {
    let mut released = key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    released.kind = KeyEventKind::Release;

    assert_eq!(
        EscapeState::Normal.handle_key(released),
        (EscapeState::Normal, InputAction::Ignore)
    );
}

#[test]
fn down_buffer_caps_growth() {
    let mut down = DownBuffer::new();
    let chunk = vec![b'x'; MAX_DOWN_BUFFER_BYTES + 32];

    down.push(&chunk);

    assert_eq!(down.bytes.len(), MAX_DOWN_BUFFER_BYTES);
    assert_eq!(down.writable().len(), MAX_DOWN_BUFFER_BYTES);

    down.consume(16);
    assert_eq!(down.bytes.len(), MAX_DOWN_BUFFER_BYTES - 16);
}
