use crate::protocol::Edge;

#[derive(Debug, Clone, Copy, Default)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
}

pub struct CursorTracker {
    position: CursorPosition,
    width: i32,
    height: i32,
}

impl CursorTracker {
    pub fn new(width: i32, height: i32) -> Self {
        let width = width.max(1);
        let height = height.max(1);

        Self {
            position: CursorPosition {
                x: width / 2,
                y: height / 2,
            },
            width,
            height,
        }
    }

    #[inline]
    pub fn position(&self) -> CursorPosition {
        self.position
    }

    #[inline]
    pub fn width(&self) -> i32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> i32 {
        self.height
    }

    #[inline]
    pub fn move_x(&mut self, delta: i32) -> (i32, i32) {
        let old = self.position.x;

        let min = 0i32;
        let max = self.width - 1;

        let requested = old as i64 + delta as i64;
        let clamped = requested.clamp(
            min as i64,
            max as i64,
        ) as i32;

        self.position.x = clamped;

        let applied = clamped - old;
        let remainder = delta - applied;

        (applied, remainder)
    }

    #[inline]
    pub fn move_y(&mut self, delta: i32) -> (i32, i32) {
        let old = self.position.y;

        let min = 0i32;
        let max = self.height - 1;

        let requested = old as i64 + delta as i64;
        let clamped = requested.clamp(
            min as i64,
            max as i64,
        ) as i32;

        self.position.y = clamped;

        let applied = clamped - old;
        let remainder = delta - applied;

        (applied, remainder)
    }

    /// Привести логическую позицию к реальной (если ОС её сообщает).
    #[inline]
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.position.x = x.clamp(0, self.width - 1);
        self.position.y = y.clamp(0, self.height - 1);
    }

    #[inline]
    pub fn set_horizontal_edge(
        &mut self,
        edge: Edge,
        y: i32,
    ) {
        self.position.y =
            y.clamp(0, self.height - 1);

        self.position.x = match edge {
            Edge::Left => 0,
            Edge::Right => self.width - 1,
            Edge::Top | Edge::Bottom => self.position.x,
        };
    }

    #[inline]
    pub fn set_vertical_edge(
        &mut self,
        edge: Edge,
        x: i32,
    ) {
        self.position.x =
            x.clamp(0, self.width - 1);

        self.position.y = match edge {
            Edge::Top => 0,
            Edge::Bottom => self.height - 1,
            Edge::Left | Edge::Right => self.position.y,
        };
    }

    #[inline]
    pub fn at_edge(&self, edge: Edge) -> bool {
        match edge {
            Edge::Left => self.position.x == 0,
            Edge::Right => {
                self.position.x == self.width - 1
            }
            Edge::Top => self.position.y == 0,
            Edge::Bottom => {
                self.position.y == self.height - 1
            }
        }
    }

    #[inline]
    pub fn at_edge_with_tolerance(
        &self,
        edge: Edge,
        tolerance: i32,
    ) -> bool {
        let tolerance = tolerance.max(0);

        match edge {
            Edge::Left => {
                self.position.x <= tolerance
            }

            Edge::Right => {
                self.position.x
                    >= (self.width - 1 - tolerance)
                    .max(0)
            }

            Edge::Top => {
                self.position.y <= tolerance
            }

            Edge::Bottom => {
                self.position.y
                    >= (self.height - 1 - tolerance)
                    .max(0)
            }
        }
    }

    #[inline]
    pub fn distance_to_edge(
        &self,
        edge: Edge,
    ) -> i32 {
        match edge {
            Edge::Left => self.position.x,

            Edge::Right => {
                (self.width - 1)
                    .saturating_sub(self.position.x)
            }

            Edge::Top => self.position.y,

            Edge::Bottom => {
                (self.height - 1)
                    .saturating_sub(self.position.y)
            }
        }
    }

    #[inline]
    pub fn would_cross_edge(
        &self,
        edge: Edge,
        delta: i32,
        tolerance: i32,
    ) -> bool {
        let tolerance = tolerance.max(0);

        match edge {
            Edge::Left => {
                delta < 0
                    && self.position.x <= tolerance
            }

            Edge::Right => {
                delta > 0
                    && self.position.x
                    >= self.width - 1 - tolerance
            }

            Edge::Top => {
                delta < 0
                    && self.position.y <= tolerance
            }

            Edge::Bottom => {
                delta > 0
                    && self.position.y
                    >= self.height - 1 - tolerance
            }
        }
    }
}