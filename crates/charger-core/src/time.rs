use std::time::Instant;

pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone)]
    pub struct FakeClock {
        time: Arc<Mutex<Instant>>,
    }

    impl FakeClock {
        pub fn new(start: Instant) -> Self {
            Self {
                time: Arc::new(Mutex::new(start)),
            }
        }

        pub fn advance(&self, duration: Duration) {
            let mut time = self.time.lock().unwrap();
            *time += duration;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.time.lock().unwrap()
        }
    }
}
