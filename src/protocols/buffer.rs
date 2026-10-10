use crate::protocol::WireEvent;

pub const DEFAULT_HANDOFF_CAPACITY: usize =
    128;

/*
 * ========================================================================
 * HANDOFF BUFFER
 * ========================================================================
 */

#[derive(Debug, Default)]
pub struct HandoffBuffer {
    events: Vec<WireEvent>,
}

impl HandoffBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(
                capacity.max(1),
            ),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    #[inline]
    pub fn push(
        &mut self,
        event: WireEvent,
    ) {
        self.events.push(event);
    }

    #[inline]
    pub fn extend(
        &mut self,
        events: &[WireEvent],
    ) {
        self.events
            .extend_from_slice(events);
    }

    #[inline]
    pub fn clear(&mut self) {
        self.events.clear();
    }

    #[inline]
    pub fn take(&mut self) -> Vec<WireEvent> {
        std::mem::take(
            &mut self.events,
        )
    }
}

/*
 * ========================================================================
 * HASH
 * ========================================================================
 */

#[inline]
pub fn hash_text(
    text: &str,
) -> u64 {
    let mut hash =
        0xcbf29ce484222325u64;

    for byte in
        text.as_bytes()
    {
        hash ^=
            *byte as u64;

        hash =
            hash.wrapping_mul(
                0x100000001b3,
            );
    }

    hash
}
