//! Байтовый прогресс сокета: живость считается по байтам, а не по целым
//! кадрам, и до `welcome` чтение ограничено бюджетом.

use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll, ready},
    time::Duration,
};

use pin_project_lite::pin_project;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::Instant,
};

/// Прогресс одного соединения; общий у обёртки сокета и пира.
#[derive(Debug)]
pub(crate) struct Meter {
    base: Instant,
    /// Миллисекунды от `base` до последнего чтения и записи.
    rx_at: AtomicU64,
    tx_at: AtomicU64,
    read: AtomicU64,
    /// Сколько всего байт можно прочитать; `u64::MAX` — без ограничения.
    limit: AtomicU64,
}

impl Meter {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            rx_at: AtomicU64::new(0),
            tx_at: AtomicU64::new(0),
            read: AtomicU64::new(0),
            limit: AtomicU64::new(u64::MAX),
        })
    }

    fn stamp(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn at(&self, stamp: &AtomicU64) -> Instant {
        self.base + Duration::from_millis(stamp.load(Ordering::Relaxed))
    }

    pub(crate) fn touch_rx(&self) {
        self.rx_at.store(self.stamp(), Ordering::Relaxed);
    }

    pub(crate) fn touch_tx(&self) {
        self.tx_at.store(self.stamp(), Ordering::Relaxed);
    }

    pub(crate) fn last_rx(&self) -> Instant {
        self.at(&self.rx_at)
    }

    pub(crate) fn last_tx(&self) -> Instant {
        self.at(&self.tx_at)
    }

    /// Дальше можно прочитать не больше `bytes`.
    pub(crate) fn cap_reads(&self, bytes: u64) {
        let read = self.read.load(Ordering::Relaxed);
        self.limit
            .store(read.saturating_add(bytes), Ordering::Relaxed);
    }

    pub(crate) fn lift_cap(&self) {
        self.limit.store(u64::MAX, Ordering::Relaxed);
    }

    /// Чтение упёрлось в `cap_reads`.
    pub(crate) fn exhausted(&self) -> bool {
        self.read.load(Ordering::Relaxed) > self.limit.load(Ordering::Relaxed)
    }

    fn on_read(&self, n: usize) -> io::Result<()> {
        self.touch_rx();
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        self.read.fetch_add(n, Ordering::Relaxed);
        if self.exhausted() {
            return Err(io::Error::other("read budget before welcome exceeded"));
        }
        Ok(())
    }
}

pin_project! {
    /// Сокет, отмечающий в `Meter` каждое успешное чтение и запись.
    pub(crate) struct Metered<T> {
        #[pin]
        io: T,
        meter: Arc<Meter>,
    }
}

impl<T> Metered<T> {
    pub fn new(io: T, meter: Arc<Meter>) -> Self {
        Self { io, meter }
    }
}

impl<T: AsyncRead> AsyncRead for Metered<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();
        let before = buf.filled().len();
        ready!(this.io.poll_read(cx, buf))?;
        let n = buf.filled().len() - before;
        if n > 0 {
            this.meter.on_read(n)?;
        }
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncWrite> AsyncWrite for Metered<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let n = ready!(this.io.poll_write(cx, buf))?;
        if n > 0 {
            this.meter.touch_tx();
        }
        Poll::Ready(Ok(n))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let n = ready!(this.io.poll_write_vectored(cx, bufs))?;
        if n > 0 {
            this.meter.touch_tx();
        }
        Poll::Ready(Ok(n))
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().io.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().io.poll_shutdown(cx)
    }
}
