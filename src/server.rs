use std::{
    collections::HashMap,
    net::SocketAddr,
};

use anyhow::{
    Context,
    Result,
};
use tokio::{
    net::{
        TcpListener,
        TcpStream,
    },
    sync::mpsc,
};

use crate::{
    config::{
        ServerConfig,
        CLIPBOARD_POLL_INTERVAL,
    },
    cursor::CursorTracker,
    input,
    protocol::{
        map_coordinate,
        read_packet,
        write_packet,
        Direction,
        Edge,
        Packet,
        Transition,
        WireEvent,
        MAX_CLIPBOARD_BYTES,
    },
    wm::{
        self,
        InputBackend,
    },
};
use crate::protocols::buffer::{HandoffBuffer, DEFAULT_HANDOFF_CAPACITY};
use crate::protocols::clipboard_sync::{
    next_clipboard,
    ClipboardSync,
    ClipboardUpdate,
};

const EDGE_TOLERANCE: i32 =
    crate::config::EDGE_TOLERANCE;

struct ConnectedClient {
    /*
     * Обычный ввод:
     *
     *   keyboard
     *   buttons
     *   wheel
     */
    input_tx: mpsc::Sender<Packet>,

    /*
     * Движение мыши.
     *
     * При перегрузке старое движение можно
     * отбросить вместо блокировки сервера.
     */
    mouse_tx: mpsc::Sender<Packet>,

    /*
     * Управляющие сообщения:
     *
     *   Hello
     *   Transition
     *
     * Всегда имеют максимальный приоритет.
     */
    control_tx: mpsc::Sender<Packet>,

    direction: Option<Direction>,
    width: i32,
    height: i32,
}

enum ClientEvent {
    Connected {
        id: u64,
        addr: SocketAddr,
        input_tx: mpsc::Sender<Packet>,
        mouse_tx: mpsc::Sender<Packet>,
        control_tx: mpsc::Sender<Packet>,
    },

    Packet {
        id: u64,
        packet: Packet,
    },

    Disconnected {
        id: u64,
    },
}

pub async fn run(
    config: ServerConfig,
) -> Result<()> {
    let listener =
        TcpListener::bind(&config.bind)
            .await
            .with_context(|| {
                format!(
                    "failed to bind {}",
                    config.bind
                )
            })?;

    eprintln!(
        "[SERVER] listening on {}",
        config.bind
    );

    /*
     * Beacon для автоматического поиска сервера в GUI клиентов.
     */
    if let Some(port) =
        config
            .bind
            .rsplit(':')
            .next()
            .and_then(|port| port.parse::<u16>().ok())
    {
        crate::discovery::spawn_beacon(port);
    }

    let mut backend =
        wm::create("Klam Server")?;

    let mut cursor =
        CursorTracker::new(
            config.screen_width,
            config.screen_height,
        );

    let (input_tx, mut input_rx) =
        mpsc::channel::<Vec<WireEvent>>(256);

    input::spawn_readers(
        Vec::new(),
        input_tx,
    )?;

    /*
     * Синхронизация буфера обмена.
     *
     * None = буфер обмена недоступен (клавиатура/мышь
     * продолжают работать).
     */
    let mut clipboard =
        ClipboardSync::spawn(
            CLIPBOARD_POLL_INTERVAL,
        );

    let (
        client_event_tx,
        mut client_event_rx,
    ) =
        mpsc::channel::<ClientEvent>(512);

    let mut clients =
        HashMap::<u64, ConnectedClient>::new();

    let mut next_id =
        1u64;

    /*
     * Кто сейчас владеет курсором.
     *
     * None = сервер
     * Some(id) = client
     */
    let mut active_client =
        None::<u64>;

    /*
     * Хвост событий при ownership handoff.
     *
     * Он используется именно в момент перехода
     * SERVER -> CLIENT.
     */
    let mut handoff =
        HandoffBuffer::new(
            DEFAULT_HANDOFF_CAPACITY,
        );

    loop {
        tokio::select! {
            accepted =
                listener.accept() => {
                let (stream, addr) =
                    accepted?;

                let id =
                    next_id;

                next_id += 1;

                spawn_client(
                    id,
                    stream,
                    addr,
                    client_event_tx.clone(),
                );
            }

            Some(events) =
                input_rx.recv() => {
                process_local_input(
                    &mut *backend,
                    &mut cursor,
                    &mut clients,
                    &mut active_client,
                    &config,
                    &mut handoff,
                    events,
                )
                .await?;
            }

            Some(event) =
                client_event_rx.recv() => {
                process_client_event(
                    &mut *backend,
                    &mut cursor,
                    &mut clients,
                    &mut active_client,
                    &config,
                    &clipboard,
                    event,
                )
                .await?;
            }

            /*
             * Локальный буфер обмена сервера изменился ->
             * рассылаем всем клиентам.
             */
            Some(update) =
                next_clipboard(&mut clipboard) => {
                if update.text.len()
                    > MAX_CLIPBOARD_BYTES
                {
                    eprintln!(
                        "[SERVER CLIPBOARD] \
                         local clipboard too large \
                         ({} bytes), not synced",
                        update.text.len()
                    );
                } else {
                    broadcast_clipboard(
                        &clients,
                        None,
                        update.hash,
                        &update.text,
                    );
                }
            }

            else => {
                break;
            }
        }
    }

    Ok(())
}

fn spawn_client(
    id: u64,
    stream: TcpStream,
    addr: SocketAddr,
    event_tx: mpsc::Sender<ClientEvent>,
) {
    tokio::spawn(async move {
        let (mut reader, mut writer) =
            stream.into_split();

        let (
            input_tx,
            mut input_rx,
        ) =
            mpsc::channel::<Packet>(256);

        let (
            mouse_tx,
            mut mouse_rx,
        ) =
            mpsc::channel::<Packet>(128);

        let (
            control_tx,
            mut control_rx,
        ) =
            mpsc::channel::<Packet>(32);

        if event_tx
            .send(
                ClientEvent::Connected {
                    id,
                    addr,
                    input_tx:
                    input_tx.clone(),
                    mouse_tx:
                    mouse_tx.clone(),
                    control_tx:
                    control_tx.clone(),
                },
            )
            .await
            .is_err()
        {
            return;
        }

        /*
         * ============================================================
         * WRITER
         * ============================================================
         *
         * Приоритет:
         *
         *   1. control
         *   2. normal input
         *   3. mouse
         */
        let writer_task =
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;

                        packet =
                            control_rx.recv() => {
                            match packet {
                                Some(packet) => {
                                    if write_packet(
                                        &mut writer,
                                        &packet,
                                    )
                                    .await
                                    .is_err()
                                    {
                                        break;
                                    }
                                }

                                None => {
                                    break;
                                }
                            }
                        }

                        packet =
                            input_rx.recv() => {
                            match packet {
                                Some(packet) => {
                                    if write_packet(
                                        &mut writer,
                                        &packet,
                                    )
                                    .await
                                    .is_err()
                                    {
                                        break;
                                    }
                                }

                                None => {
                                    break;
                                }
                            }
                        }

                        packet =
                            mouse_rx.recv() => {
                            match packet {
                                Some(packet) => {
                                    if write_packet(
                                        &mut writer,
                                        &packet,
                                    )
                                    .await
                                    .is_err()
                                    {
                                        break;
                                    }
                                }

                                None => {
                                    break;
                                }
                            }
                        }

                        else => {
                            break;
                        }
                    }
                }
            });

        /*
         * ============================================================
         * READER
         * ============================================================
         */
        loop {
            match read_packet(
                &mut reader,
            )
                .await
            {
                Ok(packet) => {
                    if event_tx
                        .send(
                            ClientEvent::Packet {
                                id,
                                packet,
                            },
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }

                Err(error) => {
                    eprintln!(
                        "[SERVER CLIENT#{}] read error: {}",
                        id,
                        error
                    );

                    break;
                }
            }
        }

        let _ =
            event_tx
                .send(
                    ClientEvent::Disconnected {
                        id,
                    },
                )
                .await;

        writer_task.abort();
    });
}

async fn process_client_event(
    backend: &mut dyn InputBackend,
    cursor: &mut CursorTracker,
    clients: &mut HashMap<u64, ConnectedClient>,
    active_client: &mut Option<u64>,
    config: &ServerConfig,
    clipboard: &Option<ClipboardSync>,
    event: ClientEvent,
) -> Result<()> {
    match event {
        ClientEvent::Connected {
            id,
            addr,
            input_tx,
            mouse_tx,
            control_tx,
        } => {
            eprintln!(
                "[SERVER ACCEPT] client#{} connected from {}",
                id,
                addr
            );

            control_tx
                .send(Packet::Hello)
                .await
                .ok();

            clients.insert(
                id,
                ConnectedClient {
                    input_tx,
                    mouse_tx,
                    control_tx,
                    direction: None,
                    width: 0,
                    height: 0,
                },
            );
        }

        ClientEvent::Packet {
            id,
            packet,
        } => {
            handle_client_packet(
                backend,
                cursor,
                clients,
                active_client,
                config,
                clipboard,
                id,
                packet,
            )
                .await?;
        }

        ClientEvent::Disconnected {
            id,
        } => {
            let was_active =
                *active_client == Some(id);

            clients.remove(&id);

            if was_active {
                *active_client =
                    None;

                eprintln!(
                    "[SERVER STATE] \
                     active client disconnected"
                );
            }

            eprintln!(
                "[SERVER DISCONNECT] \
                 client#{}",
                id
            );
        }
    }

    Ok(())
}

async fn handle_client_packet(
    backend: &mut dyn InputBackend,
    cursor: &mut CursorTracker,
    clients: &mut HashMap<u64, ConnectedClient>,
    active_client: &mut Option<u64>,
    config: &ServerConfig,
    clipboard: &Option<ClipboardSync>,
    id: u64,
    packet: Packet,
) -> Result<()> {
    match packet {
        /*
         * ============================================================
         * REGISTER
         * ============================================================
         */
        Packet::Register {
            direction,
            width,
            height,
        } => {
            let duplicate =
                clients
                    .values()
                    .any(|client| {
                        client.direction
                            == Some(direction)
                    });

            if duplicate {
                if let Some(client) =
                    clients.get(&id)
                {
                    client
                        .control_tx
                        .send(Packet::Error(
                            "direction already occupied"
                                .into(),
                        ))
                        .await
                        .ok();
                }

                return Ok(());
            }

            if let Some(client) =
                clients.get_mut(&id)
            {
                client.direction =
                    Some(direction);

                client.width =
                    width;

                client.height =
                    height;
            }

            eprintln!(
                "[SERVER REGISTER] \
                 client#{} {:?} {}x{}",
                id,
                direction,
                width,
                height
            );
        }

        /*
         * ============================================================
         * CLIENT INPUT
         * ============================================================
         *
         * Старый/совместимый путь.
         *
         * В актуальной архитектуре клиент не должен отправлять
         * сюда управление ownership.
         */
        Packet::Input(events) => {
            if *active_client != Some(id) {
                return Ok(());
            }

            if !events.is_empty() {
                backend
                    .send_events(&events)
                    .context(
                        "failed to inject remote input",
                    )?;
            }
        }

        /*
         * ============================================================
         * CURSOR RETURN
         * ============================================================
         *
         * CLIENT -> SERVER
         *
         * Клиент сообщает своё логическое положение
         * около своей обратной границы.
         */
        Packet::Cursor { x, y } => {
            if *active_client != Some(id) {
                return Ok(());
            }

            let Some(client) =
                clients.get(&id)
            else {
                return Ok(());
            };

            let Some(direction) =
                client.direction
            else {
                return Ok(());
            };

            /*
             * direction = сторона CLIENT
             * относительно SERVER.
             *
             * Возврат идёт через противоположную
             * сторону CLIENT.
             */
            let return_edge =
                direction
                    .edge()
                    .opposite();

            if !at_remote_return_edge(
                return_edge,
                x,
                y,
                client.width,
                client.height,
            ) {
                return Ok(());
            }

            /*
             * Край сервера противоположен краю,
             * через который клиент возвращается.
             */
            let server_edge =
                return_edge.opposite();

            let server_coordinate =
                match server_edge {
                    Edge::Left
                    | Edge::Right => {
                        map_coordinate(
                            y,
                            client.height,
                            config.screen_height,
                        )
                    }

                    Edge::Top
                    | Edge::Bottom => {
                        map_coordinate(
                            x,
                            client.width,
                            config.screen_width,
                        )
                    }
                };

            let current =
                cursor.position();

            /*
             * Физически возвращаем курсор сервера
             * на соответствующий край.
             */
            backend
                .sync_to_edge(
                    server_edge,
                    server_coordinate,
                    current,
                )?;

            /*
             * Синхронизируем логический курсор.
             */
            match server_edge {
                Edge::Left
                | Edge::Right => {
                    cursor.set_horizontal_edge(
                        server_edge,
                        server_coordinate,
                    );
                }

                Edge::Top
                | Edge::Bottom => {
                    cursor.set_vertical_edge(
                        server_edge,
                        server_coordinate,
                    );
                }
            }

            let transition =
                match server_edge {
                    Edge::Left
                    | Edge::Right => {
                        Transition::Horizontal {
                            edge: server_edge,
                            y: server_coordinate,
                        }
                    }

                    Edge::Top
                    | Edge::Bottom => {
                        Transition::Vertical {
                            edge: server_edge,
                            x: server_coordinate,
                        }
                    }
                };

            eprintln!(
                "[SERVER TRANSITION RETURN] \
                 client#{} \
                 client_edge={:?} \
                 server_edge={:?} \
                 client_cursor=({}, {}) \
                 server_coordinate={}",
                id,
                return_edge,
                server_edge,
                x,
                y,
                server_coordinate
            );

            /*
             * Сначала отправляем подтверждение возврата.
             *
             * control channel имеет приоритет перед обычным
             * и mouse traffic.
             */
            if let Some(client) =
                clients.get(&id)
            {
                client
                    .control_tx
                    .send(
                        Packet::Transition(
                            transition,
                        ),
                    )
                    .await
                    .ok();
            }

            /*
             * После принятия возврата ownership снова
             * принадлежит серверу.
             */
            *active_client =
                None;

            eprintln!(
                "[SERVER STATE] \
                 active=None \
                 cursor={:?}",
                cursor.position()
            );
        }

        /*
         * ============================================================
         * PING
         * ============================================================
         */
        Packet::Ping => {
            if let Some(client) =
                clients.get(&id)
            {
                client
                    .control_tx
                    .send(Packet::Pong)
                    .await
                    .ok();
            }
        }

        /*
         * ============================================================
         * CLIPBOARD
         * ============================================================
         *
         * CLIENT -> SERVER -> (локальный буфер + остальные клиенты)
         */
        Packet::Clipboard { hash, text } => {
            if text.len() > MAX_CLIPBOARD_BYTES {
                return Ok(());
            }

            eprintln!(
                "[SERVER CLIPBOARD] \
                 client#{} sent {} bytes",
                id,
                text.len()
            );

            if let Some(sync) =
                clipboard.as_ref()
            {
                sync.push_remote(
                    ClipboardUpdate {
                        hash,
                        text: text.clone(),
                    },
                );
            }

            broadcast_clipboard(
                clients,
                Some(id),
                hash,
                &text,
            );
        }

        Packet::Pong
        | Packet::Hello
        | Packet::Transition(_)
        | Packet::Error(_) => {}
    }

    Ok(())
}

async fn process_local_input(
    backend: &mut dyn InputBackend,
    cursor: &mut CursorTracker,
    clients: &mut HashMap<u64, ConnectedClient>,
    active_client: &mut Option<u64>,
    config: &ServerConfig,
    handoff: &mut HandoffBuffer,
    events: Vec<WireEvent>,
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    /*
     * ================================================================
     * REMOTE OWNERSHIP
     * ================================================================
     *
     * Курсор сейчас принадлежит CLIENT.
     *
     * Сервер:
     *
     *   keyboard/buttons/wheel -> normal channel
     *   mouse movement         -> mouse channel
     *
     * Здесь сервер не изменяет свой CursorTracker.
     */
    if let Some(id) =
        *active_client
    {
        let Some(client) =
            clients.get(&id)
        else {
            *active_client =
                None;

            return Ok(());
        };

        let mut normal =
            Vec::with_capacity(
                events.len(),
            );

        let mut mouse =
            Vec::with_capacity(
                events.len(),
            );

        for event in events {
            if is_rel_x(event)
                || is_rel_y(event)
            {
                mouse.push(event);
            } else {
                normal.push(event);
            }
        }

        /*
         * Keyboard / buttons / wheel.
         */
        if !normal.is_empty() {
            client
                .input_tx
                .send(Packet::Input(normal))
                .await
                .ok();
        }

        /*
         * Mouse.
         *
         * Здесь допускается потеря старого движения
         * при перегрузке.
         */
        if !mouse.is_empty() {
            let _ =
                client
                    .mouse_tx
                    .try_send(
                        Packet::Input(mouse),
                    );
        }

        return Ok(());
    }

    /*
     * ================================================================
     * LOCAL SERVER OWNERSHIP
     * ================================================================
     *
     * Пока сервер владеет курсором:
     *
     *   CursorTracker = logical state
     *   physical REL = original event
     *
     * Никакого adjusted.value = applied.
     *
     * Это не создаёт дополнительной физической стены.
     */

    let mut local =
        Vec::with_capacity(
            events.len(),
        );

    for (index, event) in
        events.iter().enumerate()
    {
        /*
         * ------------------------------------------------------------
         * X
         * ------------------------------------------------------------
         */
        if is_rel_x(*event) {
            let position =
                cursor.position();

            /*
             * LEFT
             *
             * Переход возможен только при движении
             * в сторону LEFT.
             *
             * Сам CursorTracker здесь ещё не двигаем.
             */
            if event.value < 0
                && position.x <= EDGE_TOLERANCE
            {
                let entered =
                    enter_client(
                        backend,
                        cursor,
                        clients,
                        active_client,
                        config,
                        Edge::Left,
                        position.y,
                    )
                        .await?;

                if entered {
                    /*
                     * Отправляем всё, что было ДО
                     * transition event.
                     */
                    emit_local_batch(
                        backend,
                        &local,
                    )?;

                    /*
                     * Текущий transition event не
                     * replay'им локально:
                     *
                     * enter_client() уже выставил
                     * физический курсор точно на край.
                     */

                    handoff.clear();

                    /*
                     * Всё, что осталось в этой batch,
                     * передаём новому владельцу через
                     * HandoffBuffer.
                     */
                    handoff.extend(
                        &events[index + 1..],
                    );

                    flush_handoff_buffer(
                        clients,
                        active_client,
                        handoff,
                    )
                        .await?;

                    return Ok(());
                }
            }

            /*
             * RIGHT
             */
            if event.value > 0
                && position.x
                >= (
                config.screen_width
                    - 1
                    - EDGE_TOLERANCE
            )
                .max(0)
            {
                let entered =
                    enter_client(
                        backend,
                        cursor,
                        clients,
                        active_client,
                        config,
                        Edge::Right,
                        position.y,
                    )
                        .await?;

                if entered {
                    emit_local_batch(
                        backend,
                        &local,
                    )?;

                    handoff.clear();

                    handoff.extend(
                        &events[index + 1..],
                    );

                    flush_handoff_buffer(
                        clients,
                        active_client,
                        handoff,
                    )
                        .await?;

                    return Ok(());
                }
            }

            /*
             * Обычное движение.
             *
             * Логическое состояние обновляем отдельно.
             * Физическое событие оставляем без изменений.
             */
            cursor.move_x(
                event.value,
            );

            local.push(*event);

            continue;
        }

        /*
         * ------------------------------------------------------------
         * Y
         * ------------------------------------------------------------
         */
        if is_rel_y(*event) {
            let position =
                cursor.position();

            /*
             * TOP
             */
            if event.value < 0
                && position.y <= EDGE_TOLERANCE
            {
                let entered =
                    enter_client(
                        backend,
                        cursor,
                        clients,
                        active_client,
                        config,
                        Edge::Top,
                        position.x,
                    )
                        .await?;

                if entered {
                    emit_local_batch(
                        backend,
                        &local,
                    )?;

                    handoff.clear();

                    handoff.extend(
                        &events[index + 1..],
                    );

                    flush_handoff_buffer(
                        clients,
                        active_client,
                        handoff,
                    )
                        .await?;

                    return Ok(());
                }
            }

            /*
             * BOTTOM
             */
            if event.value > 0
                && position.y
                >= (
                config.screen_height
                    - 1
                    - EDGE_TOLERANCE
            )
                .max(0)
            {
                let entered =
                    enter_client(
                        backend,
                        cursor,
                        clients,
                        active_client,
                        config,
                        Edge::Bottom,
                        position.x,
                    )
                        .await?;

                if entered {
                    emit_local_batch(
                        backend,
                        &local,
                    )?;

                    handoff.clear();

                    handoff.extend(
                        &events[index + 1..],
                    );

                    flush_handoff_buffer(
                        clients,
                        active_client,
                        handoff,
                    )
                        .await?;

                    return Ok(());
                }
            }

            /*
             * Обычное вертикальное движение.
             */
            cursor.move_y(
                event.value,
            );

            local.push(*event);

            continue;
        }

        /*
         * Keyboard / buttons / wheel / SYN_REPORT.
         */
        local.push(*event);
    }

    /*
     * Обычная локальная batch.
     */
    emit_local_batch(
        backend,
        &local,
    )?;

    /*
     * Подтягиваем логический курсор к реальному
     * (Windows: родная скорость и акселерация).
     */
    if let Some(real) =
        backend.cursor_position()
    {
        cursor.set_position(
            real.x,
            real.y,
        );
    }

    Ok(())
}

async fn enter_client(
    backend: &mut dyn InputBackend,
    cursor: &mut CursorTracker,
    clients: &HashMap<u64, ConnectedClient>,
    active_client: &mut Option<u64>,
    config: &ServerConfig,
    edge: Edge,
    coordinate: i32,
) -> Result<bool> {
    let direction =
        match edge {
            Edge::Left =>
                Direction::Left,

            Edge::Right =>
                Direction::Right,

            Edge::Top =>
                Direction::Top,

            Edge::Bottom =>
                Direction::Bottom,
        };

    /*
     * Ищем клиента для конкретного направления.
     */
    let Some((&id, client)) =
        clients
            .iter()
            .find(|(_, client)| {
                client.direction
                    == Some(direction)
            })
    else {
        /*
         * Клиента нет.
         *
         * Нельзя менять ownership.
         */
        return Ok(false);
    };

    if *active_client ==
        Some(id)
    {
        return Ok(false);
    }

    /*
     * Переводим координату seam между экранами.
     */
    let mapped =
        match edge {
            Edge::Left
            | Edge::Right => {
                map_coordinate(
                    coordinate,
                    config.screen_height,
                    client.height,
                )
            }

            Edge::Top
            | Edge::Bottom => {
                map_coordinate(
                    coordinate,
                    config.screen_width,
                    client.width,
                )
            }
        };

    let current =
        cursor.position();

    /*
     * Сначала физически выставляем серверный курсор
     * точно на границу.
     */
    backend
        .sync_to_edge(
            edge,
            coordinate,
            current,
        )?;

    /*
     * Затем приводим logical state в то же состояние.
     */
    match edge {
        Edge::Left
        | Edge::Right => {
            cursor.set_horizontal_edge(
                edge,
                coordinate,
            );
        }

        Edge::Top
        | Edge::Bottom => {
            cursor.set_vertical_edge(
                edge,
                coordinate,
            );
        }
    }

    let transition =
        match edge {
            Edge::Left
            | Edge::Right => {
                Transition::Horizontal {
                    edge,
                    y: mapped,
                }
            }

            Edge::Top
            | Edge::Bottom => {
                Transition::Vertical {
                    edge,
                    x: mapped,
                }
            }
        };

    eprintln!(
        "[SERVER TRANSITION OUT] \
         {:?} -> client#{} {:?}",
        transition,
        id,
        direction
    );

    /*
     * Transition всегда идёт через control channel.
     */
    client
        .control_tx
        .send(
            Packet::Transition(
                transition,
            ),
        )
        .await
        .context(
            "failed to send transition",
        )?;

    /*
     * После успешной отправки ownership меняется.
     */
    *active_client =
        Some(id);

    eprintln!(
        "[SERVER STATE] \
         active={:?} \
         cursor={:?}",
        active_client,
        cursor.position()
    );

    Ok(true)
}

async fn flush_handoff_buffer(
    clients: &HashMap<u64, ConnectedClient>,
    active_client: &Option<u64>,
    handoff: &mut HandoffBuffer,
) -> Result<()> {
    let Some(id) =
        *active_client
    else {
        handoff.clear();
        return Ok(());
    };

    let Some(client) =
        clients.get(&id)
    else {
        handoff.clear();
        return Ok(());
    };

    let events =
        handoff.take();

    if events.is_empty() {
        return Ok(());
    }

    /*
     * Handoff отправляется ОДНИМ Packet::Input.
     *
     * Это важно:
     *
     *   - сохраняется исходный порядок;
     *   - mouse events не проходят через try_send();
     *   - хвост transition batch не теряется;
     *   - Transition уже был отправлен через control channel.
     */
    client
        .input_tx
        .send(Packet::Input(events))
        .await
        .context(
            "failed to flush handoff buffer",
        )?;

    Ok(())
}

fn emit_local_batch(
    backend: &mut dyn InputBackend,
    events: &[WireEvent],
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    backend
        .send_events(events)
        .context(
            "failed to replay local input",
        )
}

fn at_remote_return_edge(
    edge: Edge,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> bool {
    match edge {
        Edge::Left =>
            x <= EDGE_TOLERANCE,

        Edge::Right =>
            x >= (
                width
                    - 1
                    - EDGE_TOLERANCE
            )
                .max(0),

        Edge::Top =>
            y <= EDGE_TOLERANCE,

        Edge::Bottom =>
            y >= (
                height
                    - 1
                    - EDGE_TOLERANCE
            )
                .max(0),
    }
}

#[inline]
fn is_rel_x(
    event: WireEvent,
) -> bool {
    event.is_rel_x()
}

#[inline]
fn is_rel_y(
    event: WireEvent,
) -> bool {
    event.is_rel_y()
}

#[allow(dead_code)]
#[inline]
fn is_mouse_axis(
    event: WireEvent,
) -> bool {
    is_rel_x(event)
        || is_rel_y(event)
}

/// Рассылает буфер обмена клиентам (кроме `except`).
///
/// `try_send`: медленный клиент не должен блокировать сервер.
fn broadcast_clipboard(
    clients: &HashMap<u64, ConnectedClient>,
    except: Option<u64>,
    hash: u64,
    text: &str,
) {
    for (id, client) in clients {
        if Some(*id) == except {
            continue;
        }

        if client.direction.is_none() {
            continue;
        }

        let _ =
            client
                .control_tx
                .try_send(
                    Packet::Clipboard {
                        hash,
                        text: text.to_owned(),
                    },
                );
    }
}
