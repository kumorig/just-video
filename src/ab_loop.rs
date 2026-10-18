//! A-B loop: repeat a section of the playing video.
//!
//! One button steps through the states: first press sets A, second sets B and
//! starts looping, third clears. The player asks [`AbLoop::jump`] each frame.

/// B must be at least this far after A, or a double press would make a
/// zero-length loop that seeks every frame.
const MIN_LENGTH: f64 = 1.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AbLoop {
    a: Option<f64>,
    b: Option<f64>,
}

impl AbLoop {
    /// The button pressed at `position` (seconds).
    pub fn press(&mut self, position: f64) {
        match (self.a, self.b) {
            (None, _) => self.a = Some(position),
            // Too close to A: keep waiting for a real B.
            (Some(a), None) if position < a + MIN_LENGTH => {}
            (Some(_), None) => self.b = Some(position),
            (Some(_), Some(_)) => *self = Self::default(),
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Where to seek now, if the video has played past B.
    pub fn jump(&self, position: f64) -> Option<f64> {
        match (self.a, self.b) {
            (Some(a), Some(b)) if position >= b => Some(a),
            _ => None,
        }
    }

    pub fn a(&self) -> Option<f64> {
        self.a
    }

    pub fn b(&self) -> Option<f64> {
        self.b
    }

    pub fn is_active(&self) -> bool {
        self.a.is_some() && self.b.is_some()
    }

    /// Small label under the bar button.
    pub fn label(&self) -> &'static str {
        match (self.a, self.b) {
            (None, _) => "Loop",
            (Some(_), None) => "A set",
            (Some(_), Some(_)) => "A-B on",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_presses_set_a_then_b_then_clear() {
        let mut l = AbLoop::default();
        assert_eq!(l.label(), "Loop");
        l.press(10.0);
        assert_eq!((l.a(), l.b(), l.is_active()), (Some(10.0), None, false));
        assert_eq!(l.jump(500.0), None, "no loop until B is set");
        l.press(40.0);
        assert!(l.is_active());
        assert_eq!(l.label(), "A-B on");
        l.press(50.0);
        assert_eq!(l, AbLoop::default());
    }

    #[test]
    fn jumps_back_to_a_at_b() {
        let mut l = AbLoop::default();
        l.press(10.0);
        l.press(40.0);
        assert_eq!(l.jump(39.9), None);
        assert_eq!(l.jump(40.0), Some(10.0));
        assert_eq!(l.jump(45.0), Some(10.0), "seeking past B also returns");
        assert_eq!(l.jump(5.0), None, "before A plays on until B");
    }

    #[test]
    fn b_too_close_to_a_is_ignored() {
        let mut l = AbLoop::default();
        l.press(10.0);
        l.press(10.2);
        assert_eq!(l.b(), None);
        l.press(12.0);
        assert_eq!(l.b(), Some(12.0));
    }
}
