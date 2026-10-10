use std::io::{self, Cursor, Error, ErrorKind};
use clap::ValueEnum;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_PACKET_SIZE: usize = 1024 * 1024;
pub const MAX_INPUT_EVENTS: usize = 4096;

/// Максимальный размер текста буфера обмена (UTF-8 байт).
pub const MAX_CLIPBOARD_BYTES: usize = MAX_PACKET_SIZE - 64;

/// Коды Linux evdev — это и есть wire-формат ввода. Windows-реализация
/// транслирует свои события в эти коды и обратно (см. `keymap`).
pub mod ev {
    #![allow(dead_code)]

    pub const EV_SYN: u16 = 0x00;
    pub const EV_KEY: u16 = 0x01;
    pub const EV_REL: u16 = 0x02;

    pub const REL_X: u16 = 0x00;
    pub const REL_Y: u16 = 0x01;
    pub const REL_HWHEEL: u16 = 0x06;
    pub const REL_WHEEL: u16 = 0x08;

    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Direction {
    Left,
    Right,
    Top,
    Bottom,
}

impl Direction {
    #[inline]
    pub const fn edge(self) -> Edge {
        match self {
            Self::Left => Edge::Left,
            Self::Right => Edge::Right,
            Self::Top => Edge::Top,
            Self::Bottom => Edge::Bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    #[inline]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Transition {
    Horizontal {
        edge: Edge,
        y: i32,
    },
    Vertical {
        edge: Edge,
        x: i32,
    },
}

impl Transition {
    #[inline]
    pub const fn edge(self) -> Edge {
        match self {
            Self::Horizontal { edge, .. } => edge,
            Self::Vertical { edge, .. } => edge,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WireEvent {
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
}

impl WireEvent {
    #[inline]
    pub const fn new(
        event_type: u16,
        code: u16,
        value: i32,
    ) -> Self {
        Self {
            event_type,
            code,
            value,
        }
    }
}

impl WireEvent {
    #[inline]
    pub const fn is_rel_x(self) -> bool {
        self.event_type == ev::EV_REL
            && self.code == ev::REL_X
    }

    #[inline]
    pub const fn is_rel_y(self) -> bool {
        self.event_type == ev::EV_REL
            && self.code == ev::REL_Y
    }
}

#[derive(Debug)]
pub enum Packet {
    Hello,

    Register {
        direction: Direction,
        width: i32,
        height: i32,
    },

    Input(Vec<WireEvent>),

    Transition(Transition),

    Ping,
    Pong,

    Error(String),

    // Абсолютная позиция курсора клиента.
    //
    // Отправляется клиентом серверу ТОЛЬКО когда клиент
    // является владельцем курсора.
    Cursor {
        x: i32,
        y: i32,
    },

    // Текстовый буфер обмена (UTF-8, переводы строк — `\n`).
    //
    // `hash` — хэш текста; получатель использует его, чтобы не отправить
    // только что полученное значение обратно.
    Clipboard {
        hash: u64,
        text: String,
    },
}

impl Packet {
    fn encode_payload(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();

        match self {
            Self::Hello => {
                out.push(0);
            }

            Self::Register {
                direction,
                width,
                height,
            } => {
                out.push(1);

                out.push(match direction {
                    Direction::Left => 0,
                    Direction::Right => 1,
                    Direction::Top => 2,
                    Direction::Bottom => 3,
                });

                out.extend_from_slice(
                    &width.to_be_bytes(),
                );

                out.extend_from_slice(
                    &height.to_be_bytes(),
                );
            }

            Self::Input(events) => {
                if events.len() > MAX_INPUT_EVENTS {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "too many input events",
                    ));
                }

                out.push(2);

                out.extend_from_slice(
                    &(events.len() as u16).to_be_bytes(),
                );

                for event in events {
                    out.extend_from_slice(
                        &event.event_type.to_be_bytes(),
                    );

                    out.extend_from_slice(
                        &event.code.to_be_bytes(),
                    );

                    out.extend_from_slice(
                        &event.value.to_be_bytes(),
                    );
                }
            }

            Self::Transition(transition) => {
                out.push(3);

                match transition {
                    Transition::Horizontal { edge, y } => {
                        out.push(0);
                        out.push(edge_to_u8(*edge));
                        out.extend_from_slice(
                            &y.to_be_bytes(),
                        );
                    }

                    Transition::Vertical { edge, x } => {
                        out.push(1);
                        out.push(edge_to_u8(*edge));
                        out.extend_from_slice(
                            &x.to_be_bytes(),
                        );
                    }
                }
            }

            Self::Ping => {
                out.push(4);
            }

            Self::Pong => {
                out.push(5);
            }

            Self::Error(message) => {
                let bytes = message.as_bytes();

                if bytes.len() > u16::MAX as usize {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "error message too large",
                    ));
                }

                out.push(6);

                out.extend_from_slice(
                    &(bytes.len() as u16).to_be_bytes(),
                );

                out.extend_from_slice(bytes);
            }

            Self::Cursor { x, y } => {
                out.push(7);

                out.extend_from_slice(
                    &x.to_be_bytes(),
                );

                out.extend_from_slice(
                    &y.to_be_bytes(),
                );
            }

            Self::Clipboard { hash, text } => {
                let bytes = text.as_bytes();

                if bytes.len() > MAX_CLIPBOARD_BYTES {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "clipboard text too large",
                    ));
                }

                out.push(8);

                out.extend_from_slice(
                    &hash.to_be_bytes(),
                );

                out.extend_from_slice(
                    &(bytes.len() as u32).to_be_bytes(),
                );

                out.extend_from_slice(bytes);
            }
        }

        Ok(out)
    }

    fn decode_payload(payload: &[u8]) -> io::Result<Self> {
        let mut cursor = Cursor::new(payload);

        let tag = read_u8(&mut cursor)?;

        match tag {
            0 => Ok(Self::Hello),

            1 => {
                let direction =
                    u8_to_direction(read_u8(&mut cursor)?)?;

                let width =
                    read_i32(&mut cursor)?;

                let height =
                    read_i32(&mut cursor)?;

                if width <= 0 || height <= 0 {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "invalid screen size",
                    ));
                }

                Ok(Self::Register {
                    direction,
                    width,
                    height,
                })
            }

            2 => {
                let count =
                    read_u16(&mut cursor)? as usize;

                if count > MAX_INPUT_EVENTS {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "too many input events",
                    ));
                }

                let mut events =
                    Vec::with_capacity(count);

                for _ in 0..count {
                    let event_type =
                        read_u16(&mut cursor)?;

                    let code =
                        read_u16(&mut cursor)?;

                    let value =
                        read_i32(&mut cursor)?;

                    events.push(
                        WireEvent {
                            event_type,
                            code,
                            value,
                        },
                    );
                }

                Ok(Self::Input(events))
            }

            3 => {
                let transition_type =
                    read_u8(&mut cursor)?;

                let edge =
                    u8_to_edge(read_u8(&mut cursor)?)?;

                let coordinate =
                    read_i32(&mut cursor)?;

                match transition_type {
                    0 => Ok(Self::Transition(
                        Transition::Horizontal {
                            edge,
                            y: coordinate,
                        },
                    )),

                    1 => Ok(Self::Transition(
                        Transition::Vertical {
                            edge,
                            x: coordinate,
                        },
                    )),

                    _ => Err(Error::new(
                        ErrorKind::InvalidData,
                        "invalid transition type",
                    )),
                }
            }

            4 => Ok(Self::Ping),

            5 => Ok(Self::Pong),

            6 => {
                let len =
                    read_u16(&mut cursor)? as usize;

                let mut bytes =
                    vec![0u8; len];

                std::io::Read::read_exact(
                    &mut cursor,
                    &mut bytes,
                )?;

                let message =
                    String::from_utf8(bytes)
                        .map_err(|_| {
                            Error::new(
                                ErrorKind::InvalidData,
                                "invalid utf8 error",
                            )
                        })?;

                Ok(Self::Error(message))
            }

            7 => {
                let x =
                    read_i32(&mut cursor)?;

                let y =
                    read_i32(&mut cursor)?;

                Ok(Self::Cursor { x, y })
            }

            8 => {
                let hash =
                    read_u64(&mut cursor)?;

                let len =
                    read_u32(&mut cursor)? as usize;

                if len > MAX_CLIPBOARD_BYTES {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "clipboard text too large",
                    ));
                }

                let mut bytes =
                    vec![0u8; len];

                std::io::Read::read_exact(
                    &mut cursor,
                    &mut bytes,
                )?;

                let text =
                    String::from_utf8(bytes)
                        .map_err(|_| {
                            Error::new(
                                ErrorKind::InvalidData,
                                "invalid utf8 clipboard",
                            )
                        })?;

                Ok(Self::Clipboard { hash, text })
            }

            _ => Err(Error::new(
                ErrorKind::InvalidData,
                "unknown packet tag",
            )),
        }
    }
}

pub async fn write_packet<W>(
    writer: &mut W,
    packet: &Packet,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload =
        packet.encode_payload()?;

    if payload.len() > MAX_PACKET_SIZE {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "packet too large",
        ));
    }

    let len =
        payload.len() as u32;

    writer
        .write_all(&len.to_be_bytes())
        .await?;

    writer
        .write_all(&payload)
        .await?;

    writer.flush().await?;

    Ok(())
}

pub async fn read_packet<R>(
    reader: &mut R,
) -> io::Result<Packet>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];

    reader
        .read_exact(&mut len_buf)
        .await?;

    let len =
        u32::from_be_bytes(len_buf)
            as usize;

    if len == 0 || len > MAX_PACKET_SIZE {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "invalid packet size",
        ));
    }

    let mut payload =
        vec![0u8; len];

    reader
        .read_exact(&mut payload)
        .await?;

    Packet::decode_payload(&payload)
}

#[inline]
fn read_u8(
    cursor: &mut Cursor<&[u8]>,
) -> io::Result<u8> {
    let mut buf = [0u8; 1];

    std::io::Read::read_exact(
        cursor,
        &mut buf,
    )?;

    Ok(buf[0])
}

#[inline]
fn read_u16(
    cursor: &mut Cursor<&[u8]>,
) -> io::Result<u16> {
    let mut buf = [0u8; 2];

    std::io::Read::read_exact(
        cursor,
        &mut buf,
    )?;

    Ok(u16::from_be_bytes(buf))
}

#[inline]
fn read_i32(
    cursor: &mut Cursor<&[u8]>,
) -> io::Result<i32> {
    let mut buf = [0u8; 4];

    std::io::Read::read_exact(
        cursor,
        &mut buf,
    )?;

    Ok(i32::from_be_bytes(buf))
}

#[inline]
fn read_u32(
    cursor: &mut Cursor<&[u8]>,
) -> io::Result<u32> {
    let mut buf = [0u8; 4];

    std::io::Read::read_exact(
        cursor,
        &mut buf,
    )?;

    Ok(u32::from_be_bytes(buf))
}

#[inline]
fn read_u64(
    cursor: &mut Cursor<&[u8]>,
) -> io::Result<u64> {
    let mut buf = [0u8; 8];

    std::io::Read::read_exact(
        cursor,
        &mut buf,
    )?;

    Ok(u64::from_be_bytes(buf))
}

#[inline]
fn edge_to_u8(edge: Edge) -> u8 {
    match edge {
        Edge::Left => 0,
        Edge::Right => 1,
        Edge::Top => 2,
        Edge::Bottom => 3,
    }
}

#[inline]
fn u8_to_edge(value: u8) -> io::Result<Edge> {
    match value {
        0 => Ok(Edge::Left),
        1 => Ok(Edge::Right),
        2 => Ok(Edge::Top),
        3 => Ok(Edge::Bottom),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            "invalid edge",
        )),
    }
}

#[inline]
fn u8_to_direction(
    value: u8,
) -> io::Result<Direction> {
    match value {
        0 => Ok(Direction::Left),
        1 => Ok(Direction::Right),
        2 => Ok(Direction::Top),
        3 => Ok(Direction::Bottom),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            "invalid direction",
        )),
    }
}

#[inline]
pub fn map_coordinate(
    value: i32,
    from_size: i32,
    to_size: i32,
) -> i32 {
    if from_size <= 1
        || to_size <= 1
    {
        return 0;
    }

    let from_max =
        (from_size - 1) as i64;

    let to_max =
        (to_size - 1) as i64;

    let value =
        (value as i64)
            .clamp(0, from_max);

    (
        (
            value * to_max
                + from_max / 2
        ) / from_max
    ) as i32
}