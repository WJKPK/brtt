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
    assert_eq!(next_routable(&[0, 1], 0), 1);
    assert_eq!(next_routable(&[0, 1], 1), 0);
    assert_eq!(next_routable(&[1], 1), 1);
    assert_eq!(next_routable(&[0, 2], 0), 2);
}

#[test]
fn down_routes_keep_queued_bytes_on_their_original_core() {
    let mut routes = DownRoutes::new(&[0, 1]);
    routes.queue(b"for-core-0");
    routes.cycle();
    assert_eq!(routes.target(), 1);
    routes.queue(b"for-core-1");

    let mut core0_sent = Vec::new();
    routes
        .flush_route(0, |bytes| {
            core0_sent.extend_from_slice(bytes);
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(core0_sent, b"for-core-0");

    let mut core1_sent = Vec::new();
    routes
        .flush_route(1, |bytes| {
            core1_sent.extend_from_slice(bytes);
            Ok(bytes.len())
        })
        .unwrap();
    // Cycling only retargets; the new core's prompt comes from the
    // renderer's cache, so no extra bytes are queued for it.
    assert_eq!(core1_sent, b"for-core-1");
    assert!(!routes.has_pending());
}

#[test]
fn down_routes_partial_writes_stay_on_their_route() {
    let mut routes = DownRoutes::new(&[0, 1]);
    routes.queue(b"abcdef");
    routes
        .flush_route(0, |bytes| Ok(bytes.len().min(2)))
        .unwrap();

    routes.cycle();
    let mut core0_rest = Vec::new();
    routes
        .flush_route(0, |bytes| {
            core0_rest.extend_from_slice(bytes);
            Ok(bytes.len())
        })
        .unwrap();
    assert_eq!(core0_rest, b"cdef");
}

#[test]
fn down_routes_refresh_preserves_target_and_reports_removed() {
    let mut routes = DownRoutes::new(&[0, 1]);
    routes.cycle();
    assert_eq!(routes.target(), 1);

    // Core 1 stays the target while core 2 joins; then core 1 vanishes.
    routes.queue(b"on-1");
    let removed = routes.set_routable(&[1, 2]);
    assert!(removed.is_empty());
    assert_eq!(routes.target(), 1);

    let removed = routes.set_routable(&[2]);
    assert_eq!(removed, vec![(1, 4)]);
    assert_eq!(routes.target(), 2);
    assert_eq!(routes.routable(), vec![2]);
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
