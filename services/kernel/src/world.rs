//! Мир операции: часы и случайность. Реальный — `God`, в тестах время
//! двигает тест (`testing::Demiurge`), поэтому хендлер не зовёт системные
//! часы и `rand` напрямую.

use eva::{
    rand::{self, Rng as _},
    time::{Clock, RealTime, Timestamp},
};
use uuid::Uuid;

/// Создаёт мир на каждую сессию.
pub trait Demiurge: Send + Sync + 'static {
    type World: WorldMut + 'static;

    fn make_world(&self) -> World<Self::World>;

    /// Время без мира: служебным задачам не нужен rng, а тестовый мир
    /// тянет его из общего потока, сдвигая id следующих сессий.
    fn now(&self) -> Timestamp;
}

pub trait WorldRef: Send + Sync + Clock {}

pub trait WorldMut: WorldRef {
    fn rng(&mut self) -> impl rand::Rng;
}

pub struct World<W>(pub W);

impl<W: WorldRef> World<W> {
    pub fn now(&self) -> Timestamp {
        self.0.get()
    }

    pub const fn clock(&self) -> &impl Clock {
        &self.0
    }
}

impl<W: WorldMut> World<W> {
    pub fn rng(&mut self) -> impl rand::Rng {
        self.0.rng()
    }

    /// uuid v7: миллисекунды часов мира и 74 случайных бита.
    pub fn uuid(&mut self) -> Uuid {
        let millis = self.now().as_millis();
        let random: [u8; 10] = self.0.rng().random();
        uuid::Builder::from_unix_timestamp_millis(millis, &random).into_uuid()
    }

    /// Строковый uuid-id: `TurnId`, `RunId`, `BlobId`, `PendingId`, `CronJobId`, `SessionId`.
    pub fn mint<I: From<String>>(&mut self) -> I {
        I::from(self.uuid().to_string())
    }
}

/// Реальный мир.
pub struct God;

impl Demiurge for God {
    type World = RealWorld;

    fn make_world(&self) -> World<RealWorld> {
        World(RealWorld)
    }

    fn now(&self) -> Timestamp {
        RealWorld.get()
    }
}

pub struct RealWorld;

impl Clock for RealWorld {
    fn get(&self) -> Timestamp {
        RealTime::default().get()
    }
}

impl WorldRef for RealWorld {}

impl WorldMut for RealWorld {
    fn rng(&mut self) -> impl rand::Rng {
        rand::rng()
    }
}

/// Детерминированный мир для тестов: часы двигает тест, rng из seed.
pub mod testing {
    use std::sync::{Arc, Mutex};

    use eva::{
        rand::{self, SeedableRng as _, xoshiro::Xoshiro256StarStar},
        time::{Clock, Mock, Timestamp},
    };

    use super::{World, WorldMut, WorldRef};

    pub struct Demiurge {
        /// Клон делит время со всеми мирами: `advance` двигает их все.
        pub clock: Mock,
        rng: Arc<Mutex<Xoshiro256StarStar>>,
    }

    impl Demiurge {
        pub fn new(seed: u64) -> Self {
            Self {
                clock: Mock::default(),
                rng: Arc::new(Mutex::new(Xoshiro256StarStar::seed_from_u64(seed))),
            }
        }
    }

    impl super::Demiurge for Demiurge {
        type World = TestWorld;

        fn make_world(&self) -> World<TestWorld> {
            let mut shared = self.rng.lock().unwrap_or_else(|e| e.into_inner());
            World(TestWorld {
                clock: self.clock.clone(),
                rng: Xoshiro256StarStar::from_rng(&mut *shared),
            })
        }

        fn now(&self) -> Timestamp {
            self.clock.get()
        }
    }

    pub struct TestWorld {
        clock: Mock,
        rng: Xoshiro256StarStar,
    }

    impl Clock for TestWorld {
        fn get(&self) -> Timestamp {
            self.clock.get()
        }
    }

    impl WorldRef for TestWorld {}

    impl WorldMut for TestWorld {
        fn rng(&mut self) -> impl rand::Rng {
            &mut self.rng
        }
    }
}
