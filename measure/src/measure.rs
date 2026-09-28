use {
    crate::clock::Timestamp,
    std::{fmt, time::Duration},
};

#[derive(Debug)]
pub struct Measure {
    name: &'static str,
    start: Timestamp,
    duration: u64,
}

impl Measure {
    pub fn start(name: &'static str) -> Self {
        Self {
            name,
            start: Timestamp::now(),
            duration: 0,
        }
    }

    pub fn stop(&mut self) {
        self.duration = self.start.elapsed().as_nanos() as u64;
    }

    pub fn as_ns(&self) -> u64 {
        self.duration
    }

    pub fn as_us(&self) -> u64 {
        self.duration / 1000
    }

    pub fn as_ms(&self) -> u64 {
        self.duration / (1000 * 1000)
    }

    pub fn as_s(&self) -> f32 {
        self.duration as f32 / (1000.0f32 * 1000.0f32 * 1000.0f32)
    }

    pub fn as_duration(&self) -> Duration {
        Duration::from_nanos(self.as_ns())
    }

    pub fn end_as_ns(self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    pub fn end_as_us(self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    pub fn end_as_ms(self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn end_as_s(self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    pub fn end_as_duration(self) -> Duration {
        self.start.elapsed()
    }
}

impl fmt::Display for Measure {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.duration == 0 {
            write!(f, "{} running", self.name)
        } else if self.as_us() < 1 {
            write!(f, "{} took {}ns", self.name, self.duration)
        } else if self.as_ms() < 1 {
            write!(f, "{} took {}us", self.name, self.as_us())
        } else if self.as_s() < 1. {
            write!(f, "{} took {}ms", self.name, self.as_ms())
        } else {
            write!(f, "{} took {:.1}s", self.name, self.as_s())
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::thread::sleep};

    #[test]
    fn test_measure() {
        let test_duration = Duration::from_millis(100);
        let mut measure = Measure::start("test");
        sleep(test_duration);
        measure.stop();
        assert!(measure.as_duration() >= test_duration);
    }

    #[test]
    fn test_measure_with_cheap_timers() {
        // Same semantics with the cycle-counter clock (when this machine has one).
        let test_duration = Duration::from_millis(20);
        crate::clock::set_cheap_timers(true);
        let mut measure = Measure::start("test");
        crate::clock::set_cheap_timers(false);
        sleep(test_duration);
        measure.stop();
        let ns = measure.as_ns();
        assert!(ns >= test_duration.as_nanos() as u64 * 97 / 100, "{ns}");
        assert_eq!(measure.as_us(), ns / 1000);
        assert_eq!(measure.as_duration(), Duration::from_nanos(ns));
        crate::clock::set_cheap_timers(true);
        let (_, us) = crate::measure_us!(sleep(test_duration));
        let (_, duration) = crate::meas_dur!(sleep(test_duration));
        crate::clock::set_cheap_timers(false);
        assert!(us >= 19_000, "{us}");
        assert!(duration >= Duration::from_millis(19), "{duration:?}");
    }

    #[test]
    fn test_measure_as() {
        let test_duration = Duration::from_millis(100);
        let measure = Measure {
            name: "test",
            start: Timestamp::now(),
            duration: test_duration.as_nanos() as u64,
        };

        assert!(f32::abs(measure.as_s() - 0.1f32) <= f32::EPSILON);
        assert_eq!(measure.as_ms(), 100);
        assert_eq!(measure.as_us(), 100_000);
        assert_eq!(measure.as_ns(), 100_000_000);
        assert_eq!(measure.as_duration(), test_duration);
    }

    #[test]
    fn test_measure_display() {
        let measure = Measure {
            name: "test_ns",
            start: Timestamp::now(),
            duration: 1,
        };
        assert_eq!(format!("{measure}"), "test_ns took 1ns");

        let measure = Measure {
            name: "test_us",
            start: Timestamp::now(),
            duration: 1000,
        };
        assert_eq!(format!("{measure}"), "test_us took 1us");

        let measure = Measure {
            name: "test_ms",
            start: Timestamp::now(),
            duration: 1000 * 1000,
        };
        assert_eq!(format!("{measure}"), "test_ms took 1ms");

        let measure = Measure {
            name: "test_s",
            start: Timestamp::now(),
            duration: 1000 * 1000 * 1000,
        };
        assert_eq!(format!("{measure}"), "test_s took 1.0s");

        let measure = Measure::start("test_not_stopped");
        assert_eq!(format!("{measure}"), "test_not_stopped running");
    }
}
