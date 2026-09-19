use anyhow::{
    Context,
    Result,
};

use evdev::{
    EventType,
    InputEvent,
    RelativeAxisCode,
    uinput::VirtualDevice,
};

use crate::{
    cursor::CursorPosition,
    input,
    protocol::Edge,
    protocol::WireEvent,
};

pub struct UinputBackend {
    device: VirtualDevice,
}

impl UinputBackend {
    pub fn new(
        name: &str,
    ) -> Result<Self> {
        Ok(Self {
            device:
            input::create_virtual_device(
                name,
            )?,
        })
    }

    /*
     * Один большой REL_* пакет может быть
     * ограничен/сжат compositor/libinput.
     *
     * Поэтому для перехода используем несколько
     * последовательных движений.
     *
     * Суммарное перемещение достаточно велико,
     * чтобы гарантированно упереть указатель
     * в границу экрана.
     */
    fn edge_motion(
        axis: RelativeAxisCode,
        direction: i32,
    ) -> Vec<InputEvent> {
        const STEP: i32 =
            8192;

        const STEPS: usize =
            16;

        let mut events =
            Vec::with_capacity(
                STEPS + 1,
            );

        for _ in 0..STEPS {
            events.push(
                InputEvent::new(
                    EventType::RELATIVE.0,
                    axis.0,
                    STEP * direction,
                ),
            );
        }

        events
    }
}

impl super::InputBackend
for UinputBackend
{
    fn send_events(
        &mut self,
        events: &[WireEvent],
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let events =
            events
                .iter()
                .map(|event| {
                    InputEvent::new(
                        event.event_type,
                        event.code,
                        event.value,
                    )
                })
                .collect::<Vec<_>>();

        self.device
            .emit(&events)
            .context(
                "failed to emit input events",
            )?;

        Ok(())
    }

    fn sync_to_edge(
        &mut self,
        edge: Edge,
        coordinate: i32,
        current: CursorPosition,
    ) -> Result<()> {
        /*
         * Не используем один REL=32767.
         *
         * Сначала аккуратно выставляем координату
         * по вторичной оси.
         */
        let coordinate =
            coordinate.max(0);

        let mut events =
            Vec::with_capacity(
                18,
            );

        match edge {
            Edge::Left => {
                let dy =
                    coordinate
                        .saturating_sub(
                            current.y,
                        );

                if dy != 0 {
                    events.push(
                        InputEvent::new(
                            EventType::RELATIVE.0,
                            RelativeAxisCode::REL_Y.0,
                            dy,
                        ),
                    );
                }

                /*
                 * Гарантированно движемся влево.
                 */
                events.extend(
                    Self::edge_motion(
                        RelativeAxisCode::REL_X,
                        -1,
                    ),
                );
            }

            Edge::Right => {
                let dy =
                    coordinate
                        .saturating_sub(
                            current.y,
                        );

                if dy != 0 {
                    events.push(
                        InputEvent::new(
                            EventType::RELATIVE.0,
                            RelativeAxisCode::REL_Y.0,
                            dy,
                        ),
                    );
                }

                /*
                 * Гарантированно движемся вправо.
                 */
                events.extend(
                    Self::edge_motion(
                        RelativeAxisCode::REL_X,
                        1,
                    ),
                );
            }

            Edge::Top => {
                let dx =
                    coordinate
                        .saturating_sub(
                            current.x,
                        );

                if dx != 0 {
                    events.push(
                        InputEvent::new(
                            EventType::RELATIVE.0,
                            RelativeAxisCode::REL_X.0,
                            dx,
                        ),
                    );
                }

                /*
                 * Гарантированно движемся вверх.
                 */
                events.extend(
                    Self::edge_motion(
                        RelativeAxisCode::REL_Y,
                        -1,
                    ),
                );
            }

            Edge::Bottom => {
                let dx =
                    coordinate
                        .saturating_sub(
                            current.x,
                        );

                if dx != 0 {
                    events.push(
                        InputEvent::new(
                            EventType::RELATIVE.0,
                            RelativeAxisCode::REL_X.0,
                            dx,
                        ),
                    );
                }

                /*
                 * Гарантированно движемся вниз.
                 */
                events.extend(
                    Self::edge_motion(
                        RelativeAxisCode::REL_Y,
                        1,
                    ),
                );
            }
        }

        /*
         * Один SYN_REPORT после всей серии
         * относительных движений.
         */
        events.push(
            InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                0,
                0,
            ),
        );

        self.device
            .emit(&events)
            .context(
                "failed to synchronize cursor",
            )?;

        Ok(())
    }
}