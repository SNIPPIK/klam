use anyhow::Result;

use crate::{
    cursor::CursorPosition,
    protocol::{
        Edge,
        WireEvent,
    },
};

mod uinput;

pub trait InputBackend: Send {
    fn send_events(
        &mut self,
        events: &[WireEvent],
    ) -> Result<()>;

    fn sync_to_edge(
        &mut self,
        edge: Edge,
        coordinate: i32,
        current: CursorPosition,
    ) -> Result<()>;
}

pub fn create(
    name: &str,
) -> Result<Box<dyn InputBackend>> {
    Ok(Box::new(
        uinput::UinputBackend::new(
            name,
        )?,
    ))
}