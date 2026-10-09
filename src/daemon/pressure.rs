// SPDX-License-Identifier: GPL-3.0-only

//! How hard the pointer is pushed into an edge: GNOME Shell's `PressureBarrier`
//! (`js/ui/layout.js`), which Hot Edge uses, fed with the motion the compositor
//! reports while the pointer can't go any further.

use std::collections::VecDeque;

/// Hot Edge's `HOT_EDGE_PRESSURE_TIMEOUT`: pushes older than this are forgotten.
const WINDOW_MS: u64 = 1000;
/// GNOME counts at most this much of each push, so one fling can't trigger.
const MAX_STEP: f64 = 15.0;

#[derive(Default)]
pub struct Pressure {
    /// (milliseconds, distance)
    pushes: VecDeque<(u64, f64)>,
    total: f64,
}

impl Pressure {
    pub fn reset(&mut self) {
        self.pushes.clear();
        self.total = 0.0;
    }

    /// One motion: `across` into the edge, `along` it. Whether that is enough.
    pub fn push(&mut self, threshold: u32, time_ms: u64, across: f64, along: f64) -> bool {
        // Away from the edge: no push at all.
        if across <= 0.0 {
            return false;
        }
        let threshold = f64::from(threshold);
        if across >= threshold {
            return true;
        }
        // Sliding along the edge more than pushing into it.
        if along.abs() > across {
            return false;
        }
        while self
            .pushes
            .front()
            .is_some_and(|(time, _)| *time + WINDOW_MS < time_ms)
        {
            let (_, distance) = self.pushes.pop_front().unwrap();
            self.total -= distance;
        }
        let distance = across.min(MAX_STEP);
        self.pushes.push_back((time_ms, distance));
        self.total += distance;
        self.total >= threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_push_triggers() {
        let mut pressure = Pressure::default();
        let hits = (0..20)
            .map(|i| pressure.push(150, i * 10, 10.0, 0.0))
            .position(|hit| hit);
        // 15 pushes of 10 px make 150.
        assert_eq!(hits, Some(14));
    }

    #[test]
    fn big_pushes_count_up_to_15() {
        let mut pressure = Pressure::default();
        assert!(!pressure.push(150, 0, 100.0, 0.0));
        assert_eq!(pressure.total, 15.0);
        assert!(pressure.push(150, 0, 150.0, 0.0));
    }

    #[test]
    fn sliding_and_old_pushes_dont_count() {
        let mut pressure = Pressure::default();
        assert!(!pressure.push(30, 0, 5.0, 20.0));
        assert_eq!(pressure.total, 0.0);
        assert!(!pressure.push(30, 0, 15.0, 0.0));
        assert!(!pressure.push(30, 2000, 10.0, 0.0));
        assert_eq!(pressure.total, 10.0);
        assert!(!pressure.push(30, 2000, -50.0, 0.0));
    }
}
