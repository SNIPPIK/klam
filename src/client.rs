use std::time::Duration;
use anyhow::{
    Context,
    Result,
};
use evdev::{
    EventType,
    RelativeAxisCode,
};
use tokio::{
    net::TcpStream,
    sync::mpsc,
};

use crate::{
    config::EDGE_TOLERANCE,
    cursor::CursorTracker,
    input,
    protocol::{
        Direction,
        Edge,
        Packet,
        Transition,
        WireEvent,
        read_packet,
        write_packet,
    },
    wm::{
        self,
        InputBackend,
    },
};
use crate::protocols::buffer::{HandoffBuffer, DEFAULT_HANDOFF_CAPACITY};

pub async fn run(
    connect: String,
    direction: Direction,
    screen_width: Option<i32>,
    screen_height: Option<i32>,
) -> Result<()> {
    let width =
        screen_width.unwrap_or(1920);

    let height =
        screen_height.unwrap_or(1080);

    let stream =
        TcpStream::connect(&connect)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to {}",
                    connect
                )
            })?;

    eprintln!(
        "[CLIENT] connected to {}",
        connect
    );

    let (
        mut reader,
        mut writer,
    ) =
        stream.into_split();

    /*
     * ================================================================
     * CLIENT -> SERVER
     * ================================================================
     *
     * Register
     * Cursor
     * Handoff Input
     *
     * Один канал гарантирует порядок:
     *
     *     Handoff Input
     *     Cursor
     */
    let (
        control_tx,
        mut control_rx,
    ) =
        mpsc::channel::<Packet>(64);

    /*
     * ================================================================
     * SERVER -> CLIENT
     * ================================================================
     */
    let (
        net_tx,
        mut net_rx,
    ) =
        mpsc::channel::<Packet>(256);

    /*
     * ================================================================
     * WRITER
     * ================================================================
     */
    tokio::spawn(async move {
        while let Some(packet) =
            control_rx.recv().await
        {
            if let Err(error) =
                write_packet(
                    &mut writer,
                    &packet,
                )
                    .await
            {
                eprintln!(
                    "[CLIENT] network write error: {}",
                    error
                );

                break;
            }
        }
    });

    /*
     * ================================================================
     * READER
     * ================================================================
     */
    tokio::spawn(async move {
        loop {
            match read_packet(
                &mut reader,
            )
                .await
            {
                Ok(packet) => {
                    if net_tx
                        .send(packet)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }

                Err(error) => {
                    eprintln!(
                        "[CLIENT] network read error: {}",
                        error
                    );

                    break;
                }
            }
        }
    });

    /*
     * ================================================================
     * REGISTER
     * ================================================================
     */
    control_tx
        .send(Packet::Register {
            direction,
            width,
            height,
        })
        .await
        .context(
            "failed to send register",
        )?;

    /*
     * ================================================================
     * INPUT
     * ================================================================
     */
    let (
        input_tx,
        mut input_rx,
    ) =
        mpsc::channel::<Vec<WireEvent>>(256);

    input::spawn_readers(
        Vec::new(),
        input_tx,
    );

    let mut backend =
        wm::create("Klam Client")?;

    let mut clipboard_tick =
        tokio::time::interval(
            Duration::from_millis(100),
        );

    let mut cursor =
        CursorTracker::new(
            width,
            height,
        );

    /*
     * false = ownership у SERVER
     * true  = ownership у CLIENT
     *
     * Название remote исторически оставлено.
     */
    let mut remote =
        false;

    /*
     * Блокирует повторную отправку Cursor
     * до подтверждения сервером.
     */
    let mut return_pending =
        false;

    /*
     * ================================================================
     * HANDOFF BUFFER
     * ================================================================
     *
     * Используется только при CLIENT -> SERVER.
     *
     * Сюда попадает:
     *
     *   - remainder от переходного REL_X / REL_Y;
     *   - весь хвост текущей input batch.
     */
    let mut handoff =
        HandoffBuffer::new(
            DEFAULT_HANDOFF_CAPACITY,
        );

    loop {
        tokio::select! {
            Some(events) =
                input_rx.recv() => {
                process_input(
                    &mut *backend,
                    &mut cursor,
                    &control_tx,
                    remote,
                    direction,
                    &mut return_pending,
                    &mut handoff,
                    events,
                )
                .await?;
            }

            Some(packet) =
                net_rx.recv() => {
                process_network_packet(
                    &mut *backend,
                    &mut cursor,
                    &control_tx,
                    &mut remote,
                    direction,
                    &mut return_pending,
                    packet,
                )
                .await?;
            }

            else => {
                break;
            }
        }
    }

    Ok(())
}

async fn process_input(
    backend: &mut dyn InputBackend,
    cursor: &mut CursorTracker,
    control_tx: &mpsc::Sender<Packet>,
    remote: bool,
    direction: Direction,
    return_pending: &mut bool,
    handoff: &mut HandoffBuffer,
    events: Vec<WireEvent>,
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    /*
     * ================================================================
     * LOCAL OWNERSHIP
     * ================================================================
     *
     * Курсор находится на клиенте локально.
     */
    if !remote {
        let mut local =
            Vec::with_capacity(
                events.len(),
            );

        for event in events.iter().copied() {
            let mut adjusted =
                event;

            if is_rel_x(event) {
                let (applied, _) =
                    cursor.move_x(
                        event.value,
                    );

                adjusted.value =
                    applied;
            } else if is_rel_y(event) {
                let (applied, _) =
                    cursor.move_y(
                        event.value,
                    );

                adjusted.value =
                    applied;
            }

            local.push(
                adjusted,
            );
        }

        backend
            .send_events(&local)
            .context(
                "failed to replay local client input",
            )?;

        return Ok(());
    }

    /*
     * ================================================================
     * REMOTE OWNERSHIP
     * ================================================================
     *
     * Ownership сейчас у SERVER.
     *
     * Физический input CLIENT:
     *
     *   1. двигает локальный CursorTracker;
     *   2. replay'ится локально;
     *   3. при возврате остаток движения и хвост
     *      помещаются в HandoffBuffer;
     *   4. Handoff отправляется SERVER;
     *   5. затем отправляется Cursor.
     *
     * ВАЖНО:
     *
     * Сам transition event остаётся локальному курсору.
     * Если он перелетает через край, только его remainder
     * уходит SERVER.
     */

    let return_edge =
        direction
            .edge()
            .opposite();

    let mut local =
        Vec::with_capacity(
            events.len(),
        );

    let mut return_index =
        None::<usize>;

    let mut return_remainder =
        0i32;

    for (index, event) in
        events.iter().copied().enumerate()
    {
        if is_rel_x(event) {
            let (_, remainder) =
                cursor.move_x(
                    event.value,
                );

            /*
             * Проверяем возврат только в направлении
             * обратного края.
             */
            let should_return =
                !*return_pending
                    && event.value != 0
                    && match return_edge {
                    Edge::Left =>
                        event.value < 0,

                    Edge::Right =>
                        event.value > 0,

                    _ => false,
                }
                    && cursor.at_edge_with_tolerance(
                    return_edge,
                    EDGE_TOLERANCE,
                );

            /*
             * Сам event всё ещё относится
             * к физическому курсору CLIENT.
             */
            local.push(
                event,
            );

            if should_return {
                return_index =
                    Some(index);

                return_remainder =
                    remainder;

                break;
            }

            continue;
        }

        if is_rel_y(event) {
            let (_, remainder) =
                cursor.move_y(
                    event.value,
                );

            let should_return =
                !*return_pending
                    && event.value != 0
                    && match return_edge {
                    Edge::Top =>
                        event.value < 0,

                    Edge::Bottom =>
                        event.value > 0,

                    _ => false,
                }
                    && cursor.at_edge_with_tolerance(
                    return_edge,
                    EDGE_TOLERANCE,
                );

            local.push(
                event,
            );

            if should_return {
                return_index =
                    Some(index);

                return_remainder =
                    remainder;

                break;
            }

            continue;
        }

        /*
         * Keyboard / buttons / wheel / SYN.
         */
        local.push(
            event,
        );
    }

    /*
     * ================================================================
     * Обычная локальная часть
     * ================================================================
     */
    backend
        .send_events(&local)
        .context(
            "failed to replay client input",
        )?;

    /*
     * Возврата нет.
     */
    let Some(index) =
        return_index
    else {
        return Ok(());
    };

    /*
     * ================================================================
     * HANDOFF
     * ================================================================
     *
     * Здесь начинается передача ownership.
     *
     * Если переходный REL event вылез за экран,
     * remainder продолжается уже на SERVER.
     *
     * После этого весь хвост текущей batch тоже
     * относится новому владельцу.
     */

    handoff.clear();

    /*
     * ------------------------------------------------------------
     * Remainder transition event
     * ------------------------------------------------------------
     */
    /*if return_remainder != 0 {
        handoff.push(
            events[index]
                .with_value(
                    return_remainder,
                ),
        );
    }*/

    /*
     * ------------------------------------------------------------
     * Остаток текущей input batch
     * ------------------------------------------------------------
     *
     * SYN_REPORT, keyboard, buttons и последующие
     * REL-события сохраняют исходный порядок.
     */
    if index + 1
        < events.len()
    {
        handoff.extend(
            &events[index + 1..],
        );
    }

    eprintln!(
        "[CLIENT HANDOFF] \
         edge={:?} \
         remainder={} \
         events={} \
         cursor={:?}",
        return_edge,
        return_remainder,
        handoff.len(),
        cursor.position()
    );

    /*
     * ================================================================
     * HANDOFF -> SERVER
     * ================================================================
     *
     * Handoff идёт ДО Cursor.
     *
     * Сервер ещё находится в состоянии:
     *
     *     active_client == Some(client_id)
     *
     * поэтому он может принять Packet::Input.
     */
    let handoff_events =
        handoff.take();

    if !handoff_events.is_empty() {
        control_tx
            .send(
                Packet::Input(
                    handoff_events,
                ),
            )
            .await
            .context(
                "failed to send client handoff",
            )?;
    }

    /*
     * Теперь ownership можно вернуть серверу.
     */
    send_return_cursor(
        cursor,
        control_tx,
        return_edge,
        return_pending,
    )
        .await?;

    Ok(())
}

async fn process_network_packet(
    backend: &mut dyn InputBackend,
    cursor: &mut CursorTracker,
    control_tx: &mpsc::Sender<Packet>,
    remote: &mut bool,
    direction: Direction,
    return_pending: &mut bool,
    packet: Packet,
) -> Result<()> {
    match packet {
        Packet::Hello => {
            eprintln!(
                "[CLIENT] hello received"
            );
        }

        /*
         * ============================================================
         * SERVER INPUT
         * ============================================================
         *
         * Когда ownership у CLIENT,
         * серверное движение управляет
         * клиентским курсором.
         *
         * Текущую рабочую механику
         * сохраняем без изменений.
         */
        Packet::Input(events) => {
            if !*remote
                || events.is_empty()
            {
                return Ok(());
            }

            let return_edge =
                direction
                    .edge()
                    .opposite();

            let mut return_requested =
                false;

            for event in &events {
                if is_rel_x(*event) {
                    cursor.move_x(
                        event.value,
                    );

                    if !*return_pending
                        && event.value != 0
                        && match return_edge {
                        Edge::Left =>
                            event.value < 0,

                        Edge::Right =>
                            event.value > 0,

                        _ => false,
                    }
                        && cursor
                        .at_edge_with_tolerance(
                            return_edge,
                            EDGE_TOLERANCE,
                        )
                    {
                        return_requested =
                            true;
                    }
                } else if is_rel_y(*event) {
                    cursor.move_y(
                        event.value,
                    );

                    if !*return_pending
                        && event.value != 0
                        && match return_edge {
                        Edge::Top =>
                            event.value < 0,

                        Edge::Bottom =>
                            event.value > 0,

                        _ => false,
                    }
                        && cursor
                        .at_edge_with_tolerance(
                            return_edge,
                            EDGE_TOLERANCE,
                        )
                    {
                        return_requested =
                            true;
                    }
                }
            }

            backend
                .send_events(&events)
                .context(
                    "failed to inject server input",
                )?;

            if return_requested {
                send_return_cursor(
                    cursor,
                    control_tx,
                    return_edge,
                    return_pending,
                )
                    .await?;
            }

            eprintln!(
                "[CLIENT NETWORK INPUT] \
                 events={} \
                 remote=true \
                 cursor={:?}",
                events.len(),
                cursor.position()
            );
        }

        /*
         * ============================================================
         * TRANSITION
         * ============================================================
         */
        Packet::Transition(
            transition,
        ) => {
            if !*remote {
                /*
                 * SERVER -> CLIENT
                 */
                apply_enter_transition(
                    backend,
                    cursor,
                    transition,
                )?;

                *remote =
                    true;

                /*
                 * Новый ownership cycle.
                 */
                *return_pending =
                    false;

                eprintln!(
                    "[CLIENT TRANSITION IN] \
                     {:?}",
                    transition
                );
            } else {
                /*
                 * SERVER подтверждает
                 * CLIENT -> SERVER.
                 */
                apply_return_transition(
                    backend,
                    cursor,
                    transition,
                )?;

                *remote =
                    false;

                /*
                 * Возврат подтверждён.
                 */
                *return_pending =
                    false;

                eprintln!(
                    "[CLIENT TRANSITION RETURN] \
                     {:?}",
                    transition
                );
            }

            eprintln!(
                "[CLIENT STATE] \
                 remote={} \
                 cursor={:?}",
                remote,
                cursor.position()
            );
        }

        Packet::Ping => {}

        Packet::Pong => {}

        Packet::Error(message) => {
            eprintln!(
                "[CLIENT ERROR] {}",
                message
            );
        }

        Packet::Register { .. }
        | Packet::Cursor { .. } => {}
    }

    Ok(())
}

async fn send_return_cursor(
    cursor: &CursorTracker,
    control_tx: &mpsc::Sender<Packet>,
    return_edge: Edge,
    return_pending: &mut bool,
) -> Result<()> {
    /*
     * Не отправляем повторный Cursor
     * до подтверждения SERVER.
     */
    if *return_pending {
        return Ok(());
    }

    let position =
        cursor.position();

    *return_pending =
        true;

    eprintln!(
        "[CLIENT RETURN REQUEST] \
         edge={:?} \
         cursor={:?}",
        return_edge,
        position
    );

    control_tx
        .send(
            Packet::Cursor {
                x: position.x,
                y: position.y,
            },
        )
        .await
        .context(
            "failed to send return cursor",
        )?;

    Ok(())
}

fn apply_enter_transition(
    backend: &mut dyn InputBackend,
    cursor: &mut CursorTracker,
    transition: Transition,
) -> Result<()> {
    let entry_edge =
        transition
            .edge()
            .opposite();

    let current =
        cursor.position();

    match transition {
        Transition::Horizontal {
            y,
            ..
        } => {
            backend.sync_to_edge(
                entry_edge,
                y,
                current,
            )?;

            cursor.set_horizontal_edge(
                entry_edge,
                y,
            );
        }

        Transition::Vertical {
            x,
            ..
        } => {
            backend.sync_to_edge(
                entry_edge,
                x,
                current,
            )?;

            cursor.set_vertical_edge(
                entry_edge,
                x,
            );
        }
    }

    Ok(())
}

fn apply_return_transition(
    _backend: &mut dyn InputBackend,
    _cursor: &mut CursorTracker,
    transition: Transition,
) -> Result<()> {
    /*
     * SERVER подтверждает CLIENT -> SERVER.
     *
     * Физический курсор CLIENT здесь
     * не двигаем.
     */
    eprintln!(
        "[CLIENT RETURN] \
         server_edge={:?} \
         physical cursor unchanged",
        transition.edge()
    );

    Ok(())
}

#[inline]
fn is_rel_x(
    event: WireEvent,
) -> bool {
    event.event_type
        == EventType::RELATIVE.0
        && event.code
        == RelativeAxisCode::REL_X.0
}

#[inline]
fn is_rel_y(
    event: WireEvent,
) -> bool {
    event.event_type
        == EventType::RELATIVE.0
        && event.code
        == RelativeAxisCode::REL_Y.0
}