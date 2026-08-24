use std::num::NonZeroU16;

use ratatui::layout::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalGeometry {
    cols: NonZeroU16,
    rows: NonZeroU16,
}

impl TerminalGeometry {
    pub const DEFAULT: Self = Self {
        cols: NonZeroU16::new(80).expect("80 is non-zero"),
        rows: NonZeroU16::new(24).expect("24 is non-zero"),
    };

    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        Some(Self {
            cols: NonZeroU16::new(cols)?,
            rows: NonZeroU16::new(rows)?,
        })
    }

    pub fn from_pane(area: Rect) -> Self {
        Self::new(area.width.max(1), area.height.max(1)).expect("clamped pane geometry is non-zero")
    }

    pub fn cols(self) -> u16 {
        self.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.rows.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_geometry() {
        assert_eq!(TerminalGeometry::new(0, 24), None);
        assert_eq!(TerminalGeometry::new(80, 0), None);
    }

    #[test]
    fn pane_geometry_never_becomes_zero() {
        let geometry = TerminalGeometry::from_pane(Rect::new(0, 0, 0, 0));

        assert_eq!(geometry.cols(), 1);
        assert_eq!(geometry.rows(), 1);
    }
}
