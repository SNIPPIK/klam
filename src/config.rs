use std::{
    path::PathBuf,
    time::Duration,
};

/// Максимальное отклонение курсора от края,
/// при котором разрешён переход на соседний стол.
///
/// 0 = строго сам край
/// 1 = край + 1 пиксель
/// 2 = край + 2 пикселя
/// 10 = край + 10 пикселей
pub const EDGE_TOLERANCE: i32 = 0;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: String,
    pub devices: Vec<PathBuf>,
    pub screen_width: i32,
    pub screen_height: i32,
    pub mouse_sensitivity: f32,
}
/// Как часто опрашивается локальный буфер обмена.
pub const CLIPBOARD_POLL_INTERVAL: Duration =
    Duration::from_millis(100);
