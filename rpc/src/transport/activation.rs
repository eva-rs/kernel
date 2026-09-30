//! Сокеты менеджера служб (`sd_listen_fds`): слушатель, чей адрес назван в
//! конфиге, берётся готовым вместо bind — права и группу его файла задаёт
//! socket-юнит (`SocketMode`, `SocketGroup`).

use std::{
    net::SocketAddr,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use socket2::{SockRef, Type};

use crate::transport::TransportError;

/// Первый унаследованный сокет по протоколу `sd_listen_fds`.
const LISTEN_FDS_START: RawFd = 3;

/// Унаследованные сокеты процесса; каждый забирает слушатель с тем же адресом.
#[derive(Debug, Default)]
pub struct Inherited(Vec<OwnedFd>);

/// Адрес, по которому слушатель узнаёт свой сокет.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Bound<'a> {
    Unix(&'a Path),
    Tcp(SocketAddr),
}

impl Inherited {
    /// Сокеты этого процесса, если `LISTEN_PID` — он сам; забрать их можно
    /// один раз. Окружение не правится: дочерний процесс отсеет чужие
    /// сокеты по `LISTEN_PID`.
    pub fn take() -> Result<Self, TransportError> {
        static TAKEN: AtomicBool = AtomicBool::new(false);

        let pid = std::env::var("LISTEN_PID").ok();
        let fds = std::env::var("LISTEN_FDS").ok();
        let count = passed(pid.as_deref(), fds.as_deref(), std::process::id())
            .map_err(TransportError::Activation)?;
        if count == 0 {
            return Ok(Self::default());
        }
        if TAKEN.swap(true, Ordering::AcqRel) {
            return Err(TransportError::Activation(
                "the inherited sockets are already taken".to_owned(),
            ));
        }
        (LISTEN_FDS_START..LISTEN_FDS_START.saturating_add(count))
            .map(|raw| {
                // SAFETY: менеджер служб передал эти fd процессу `LISTEN_PID`, `TAKEN` не даёт взять их дважды.
                let fd = unsafe { OwnedFd::from_raw_fd(raw) };
                // Копия получает CLOEXEC: слушатель не утечёт в дочерние процессы.
                fd.try_clone()
                    .map_err(|e| TransportError::Activation(format!("fd {raw}: {e}")))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    /// Готовые слушающие сокеты, переданные не через окружение.
    pub fn from_fds(fds: impl IntoIterator<Item = OwnedFd>) -> Self {
        Self(fds.into_iter().collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Сокет, привязанный к `bound`. Он обязан слушать: при `Accept=yes`
    /// менеджер отдаёт уже принятое соединение с тем же адресом.
    pub(crate) fn claim(&mut self, bound: Bound<'_>) -> Result<Option<OwnedFd>, TransportError> {
        let Some(at) = self.0.iter().position(|fd| bound.matches(fd)) else {
            return Ok(None);
        };
        let fd = self.0.remove(at);
        let socket = SockRef::from(&fd);
        let listening = socket.r#type().is_ok_and(|kind| kind == Type::STREAM)
            && socket.is_listener().unwrap_or(false);
        if !listening {
            return Err(TransportError::Activation(format!(
                "the inherited socket {} is not a listening stream socket: set Accept=no",
                describe(&fd)
            )));
        }
        Ok(Some(fd))
    }

    /// Сокет, который не назвал ни один слушатель, — ошибка: его некому обслужить.
    pub fn finish(self) -> Result<(), TransportError> {
        if self.0.is_empty() {
            return Ok(());
        }
        let names: Vec<_> = self.0.iter().map(describe).collect();
        Err(TransportError::Activation(format!(
            "no listener is configured at the inherited {}",
            names.join(", ")
        )))
    }
}

impl Bound<'_> {
    fn matches(self, fd: &OwnedFd) -> bool {
        let Ok(addr) = SockRef::from(fd).local_addr() else {
            return false;
        };
        match self {
            Self::Unix(path) => addr.as_pathname() == Some(path),
            Self::Tcp(want) => addr.as_socket() == Some(want),
        }
    }
}

fn describe(fd: &OwnedFd) -> String {
    let addr = SockRef::from(fd).local_addr().ok();
    match addr.as_ref().map(|a| (a.as_pathname(), a.as_socket())) {
        Some((Some(path), _)) => format!("uds://{}", path.display()),
        Some((None, Some(addr))) => format!("tcp://{addr}"),
        _ => format!("fd {}", fd.as_raw_fd()),
    }
}

/// Сколько сокетов передано этому процессу (`me`) по `LISTEN_PID`/`LISTEN_FDS`.
fn passed(pid: Option<&str>, fds: Option<&str>, me: u32) -> Result<i32, String> {
    if pid.and_then(|pid| pid.trim().parse::<u32>().ok()) != Some(me) {
        return Ok(0);
    }
    match fds.map(|n| n.trim().parse::<u16>()) {
        None => Ok(0),
        Some(Ok(n)) => Ok(i32::from(n)),
        Some(Err(_)) => Err(format!("LISTEN_FDS is not a number: {fds:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::passed;

    #[test]
    fn sockets_belong_to_the_named_process_only() {
        assert_eq!(passed(None, None, 7), Ok(0));
        assert_eq!(passed(Some("8"), Some("1"), 7), Ok(0));
        assert_eq!(passed(Some("junk"), Some("1"), 7), Ok(0));
        assert_eq!(passed(Some("7"), None, 7), Ok(0));
        assert_eq!(passed(Some("7"), Some("0"), 7), Ok(0));
        assert_eq!(passed(Some("7"), Some("2"), 7), Ok(2));
        assert!(passed(Some("7"), Some("x"), 7).is_err());
    }
}
