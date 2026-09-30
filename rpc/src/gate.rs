//! Лимит соединений, не доказавших права (`06 §2`): у слушателя клиентов —
//! до `hello`, у слушателя сервиса — до конца Noise. Сверх лимита новичок
//! вытесняет старейшее ещё молчащее соединение: молчащие сокеты лишь
//! задерживают честного пира, а не запирают ему вход.

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv6Addr},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio_util::sync::CancellationToken;

/// Соединений до доказательства прав на слушателя.
pub const MAX_PENDING: usize = 64;

/// Соединений до доказательства прав из одной сети пира (`Net`).
pub const MAX_PENDING_PER_NET: usize = 8;

/// Сеть пира: IPv4 — адрес, IPv6 — префикс /64, который обычно целиком у
/// одного хоста.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Net(IpAddr);

impl Net {
    fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(v4) => Self(IpAddr::V4(v4)),
            IpAddr::V6(v6) => {
                let prefix = u128::from(v6) & !u128::from(u64::MAX);
                Self(IpAddr::V6(Ipv6Addr::from(prefix)))
            }
        }
    }
}

/// Места для соединений до доказательства прав: всего и на одну сеть.
pub struct Gate {
    state: Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    next: u64,
    /// Места по возрасту: первое — старейшее.
    seats: BTreeMap<u64, Seat>,
}

struct Seat {
    net: Option<Net>,
    evict: CancellationToken,
    /// Пир ещё ничего не доказал: место можно отдать новичку.
    silent: bool,
}

impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Место новичку (`ip` — у tcp); при исчерпанном лимите — за счёт
    /// старейшего молчащего: из его же сети, если она выбрала свою долю,
    /// иначе любого. `None` — молчащих нет, новичка не пускать.
    pub fn admit(self: &Arc<Self>, ip: Option<IpAddr>) -> Option<Pass> {
        let net = ip.map(Net::of);
        let kin = |seat: &Seat| net.is_some() && seat.net == net;
        let mut state = self.lock();
        let crowded = state.seats.values().filter(|&seat| kin(seat)).count() >= MAX_PENDING_PER_NET;
        if crowded || state.seats.len() >= MAX_PENDING {
            let victim = state
                .seats
                .iter()
                .find(|(_, seat)| seat.silent && (!crowded || kin(seat)))
                .map(|(&key, _)| key)?;
            if let Some(seat) = state.seats.remove(&victim) {
                seat.evict.cancel();
            }
        }

        let key = state.next;
        state.next += 1;
        let evict = CancellationToken::new();
        state.seats.insert(
            key,
            Seat {
                net,
                evict: evict.clone(),
                silent: true,
            },
        );
        Some(Pass {
            gate: self.clone(),
            key,
            ip,
            evict,
        })
    }
}

/// Место соединения в `Gate`; освобождается на drop.
pub struct Pass {
    gate: Arc<Gate>,
    key: u64,
    ip: Option<IpAddr>,
    evict: CancellationToken,
}

impl Pass {
    pub fn ip(&self) -> Option<IpAddr> {
        self.ip
    }

    /// Место отдали новичку: соединение пора бросить.
    pub async fn evicted(&self) {
        self.evict.cancelled().await;
    }

    /// Пир доказал права: дальше его не вытесняют.
    pub fn spoke(&self) {
        if let Some(seat) = self.gate.lock().seats.get_mut(&self.key) {
            seat.silent = false;
        }
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        self.gate.lock().seats.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ip: &str) -> Net {
        Net::of(ip.parse().unwrap())
    }

    fn ip(ip: &str) -> Option<IpAddr> {
        Some(ip.parse().unwrap())
    }

    #[test]
    fn ipv6_hosts_share_a_net_by_64() {
        assert_eq!(net("2001:db8::1"), net("2001:db8::2"));
        assert_eq!(net("2001:db8::1"), net("2001:db8::ffff:ffff:ffff:ffff"));
        assert_ne!(net("2001:db8::1"), net("2001:db8:0:1::1"));
        assert_ne!(net("192.0.2.1"), net("192.0.2.2"));
        // IPv4 через dual-stack сокет — всё тот же адрес, а не одна /64 на всех.
        assert_eq!(net("::ffff:192.0.2.1"), net("192.0.2.1"));
        assert_ne!(net("::ffff:192.0.2.1"), net("::ffff:192.0.2.2"));
    }

    #[test]
    fn crowded_net_evicts_its_own_oldest() {
        let gate = Gate::new();
        let other = gate.admit(ip("192.0.2.9")).unwrap();
        let own: Vec<_> = (0..MAX_PENDING_PER_NET)
            .map(|n| gate.admit(ip(&format!("2001:db8::{n}"))).unwrap())
            .collect();
        let newcomer = gate.admit(ip("2001:db8::ff")).unwrap();

        assert!(own[0].evict.is_cancelled());
        assert!(own[1..].iter().all(|pass| !pass.evict.is_cancelled()));
        assert!(!other.evict.is_cancelled());
        assert!(!newcomer.evict.is_cancelled());
    }

    #[test]
    fn full_gate_evicts_the_oldest_silent_only() {
        let gate = Gate::new();
        let held: Vec<_> = (0..MAX_PENDING)
            .map(|n| gate.admit(ip(&format!("10.0.{n}.1"))).unwrap())
            .collect();
        held[0].spoke();

        let newcomer = gate.admit(ip("192.0.2.1")).unwrap();
        assert!(
            !held[0].evict.is_cancelled(),
            "authenticating is not evicted"
        );
        assert!(held[1].evict.is_cancelled());

        for pass in held.iter().chain([&newcomer]) {
            pass.spoke();
        }
        assert!(gate.admit(ip("192.0.2.2")).is_none());
        assert!(gate.admit(None).is_none());
    }
}
