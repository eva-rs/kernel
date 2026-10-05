//! Компонент `gc` (`08 §3`): еженедельный слот, одна транзакция уборки,
//! файлы blob'ов и события после коммита. Живые туры жертв отменяет `TurnCancel`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use eva::{supervisor::SlaveRx, time::Timestamp};
use tokio::time::MissedTickBehavior;

use eva_protocol::{
    types::{BlobId, ChatId, RunId},
    wire::events::{ChatPurged, Event},
};

use crate::{
    components::Components,
    config::GcConfig,
    db::{
        self, Db as _, DbResult, Queries as _,
        chats::ChatRef,
        gc::Cutoffs,
        meta::GC_LAST_STARTED,
        stats::{DayZone, Metric, ago},
    },
    handling::blobs::STAGING,
    kernel::Kernel,
    subsystems::TurnCancel,
};

/// Итог уборки: чаты и число строк по сущностям (`deleted_json`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub run: RunId,
    pub started: Timestamp,
    pub chats: Vec<ChatId>,
    pub blobs: Vec<BlobId>,
    pub deleted: BTreeMap<String, u64>,
}

pub struct Gc<C: Components, T> {
    kernel: Kernel<C>,
    turns: Arc<T>,
}

/// Слот недели, в который попадает `now` (`08 §3.1`).
pub fn slot(cfg: &GcConfig, zone: &DayZone, now: Timestamp) -> Timestamp {
    zone.latest_weekly(cfg.at.weekday.to_jiff(), cfg.at.time.0, now)
}

/// Due, пока текущий слот не убран: пропуск за даунтайм — первый тик.
pub fn is_due(
    cfg: &GcConfig,
    zone: &DayZone,
    last_started: Option<Timestamp>,
    now: Timestamp,
) -> bool {
    last_started.is_none_or(|last| last < slot(cfg, zone, now))
}

/// Gauge `next_gc_at`: ближайший слот, который ещё не убирали.
pub fn next_at(
    cfg: &GcConfig,
    zone: &DayZone,
    last_started: Option<Timestamp>,
    now: Timestamp,
) -> Timestamp {
    let current = slot(cfg, zone, now);
    if is_due(cfg, zone, last_started, now) {
        current
    } else {
        zone.next_weekly(cfg.at.weekday.to_jiff(), cfg.at.time.0, current)
    }
}

struct Swept {
    victims: Vec<ChatRef>,
    blobs: Vec<BlobId>,
    known_blobs: Vec<String>,
    deleted: BTreeMap<String, u64>,
}

impl<C: Components, T: TurnCancel> Gc<C, T> {
    pub fn new(kernel: Kernel<C>, turns: T) -> Self {
        Self {
            kernel,
            turns: Arc::new(turns),
        }
    }

    /// Уборка, если слот наступил и ещё не убран.
    pub async fn tick(&self) -> DbResult<Option<Report>> {
        let shared = self.kernel.shared();
        let now = shared.now();
        let last = shared
            .db()
            .reader()
            .await?
            .run(|c| c.meta().timestamp(GC_LAST_STARTED))
            .await?;
        if !is_due(&shared.cfg().gc, &shared.day_zone(), last, now) {
            return Ok(None);
        }
        self.collect().await.map(Some)
    }

    /// Уборка безусловно (`08 §3.3`): одна транзакция, затем файлы, отмена
    /// туров, событие `chat.purged`, checkpoint и vacuum.
    pub async fn collect(&self) -> DbResult<Report> {
        let shared = self.kernel.shared().clone();
        let cfg = shared.cfg();
        let gc = cfg.gc.clone();
        let zone = shared.day_zone();
        let started = shared.now();
        let run: RunId = shared.make_world().mint();
        let cut = Cutoffs {
            now: started,
            ephemeral_idle: ago(started, *gc.ephemeral_idle),
            deleted: ago(started, *gc.deleted_after),
            memory_archive: ago(started, *gc.memory_archive_after),
            memory_history: ago(started, *cfg.memory.history_ttl),
            embed_cache: ago(started, *gc.embed_cache_after),
            pending: ago(started, *gc.pending_after),
        };
        let stats_from = zone.day(ago(started, *cfg.stats.keep));
        let warm_from = zone.day(ago(started, cfg.tools.warm.window.0));
        let spend_from = ago(started, *cfg.stats.spend_keep);
        let keeps = (gc.cron_runs_keep, gc.sleep_runs_keep, gc.gc_runs_keep);

        let candidates = shared
            .db()
            .reader()
            .await?
            .run(move |c| c.gc().chat_victims(&cut))
            .await?;
        let mut paused: BTreeSet<_> = candidates.iter().map(|c| c.id).collect();
        let mut pauses = Vec::new();
        for chat in &paused {
            pauses.push(shared.turns().slot(*chat).pause_ingest().await);
        }
        let victims = loop {
            let victims = shared
                .db()
                .reader()
                .await?
                .run(move |c| c.gc().chat_victims(&cut))
                .await?;
            if victims.iter().all(|c| paused.contains(&c.id)) {
                break victims;
            }
            for victim in victims {
                if paused.insert(victim.id) {
                    pauses.push(shared.turns().slot(victim.id).pause_ingest().await);
                }
            }
        };
        let candidate_ids: Vec<_> = victims.iter().map(|c| c.id).collect();
        self.turns.cancel(candidate_ids).await;

        let (run_tx, shared_tx, zone_tx) = (run.clone(), shared.clone(), zone.clone());
        let swept = shared
            .db()
            .write(move |tx| -> DbResult<Swept> {
                let ids: Vec<ChatId> = victims.iter().map(|v| v.id).collect();
                let mut deleted = BTreeMap::new();
                note(&mut deleted, "chats", tx.gc().delete_chats(&ids)?);
                let blobs = tx.gc().expired_blobs(cut.now)?;
                note(&mut deleted, "blobs", tx.gc().delete_blobs(&blobs)?);
                note(
                    &mut deleted,
                    "memory_forgotten",
                    tx.gc().purge_memory_forgotten(cut.memory_archive)?,
                );
                note(
                    &mut deleted,
                    "memory_superseded",
                    tx.gc().purge_memory_superseded(cut.memory_history)?,
                );
                note(
                    &mut deleted,
                    "chat_notes",
                    tx.gc().purge_notes(cut.memory_archive)?,
                );
                note(&mut deleted, "tool_pins", tx.gc().purge_tool_pins(cut.now)?);
                note(
                    &mut deleted,
                    "chat_warm_sets",
                    tx.tools().purge_warm(&warm_from)?,
                );
                note(
                    &mut deleted,
                    "embed_cache",
                    tx.gc().purge_embed_cache(cut.embed_cache)?,
                );
                note(&mut deleted, "pending", tx.gc().purge_pending(cut.pending)?);
                let stats = tx.stats().purge(&stats_from, spend_from)?;
                note(&mut deleted, "tool_stats", stats.tool_stats);
                note(&mut deleted, "chat_stats", stats.chat_stats);
                note(&mut deleted, "health_stats", stats.health_stats);
                note(&mut deleted, "spend", stats.spend);
                note(&mut deleted, "cron_runs", tx.cron().purge_runs(keeps.0)?);
                note(&mut deleted, "sleep_runs", tx.sleep().purge_runs(keeps.1)?);
                let finished = shared_tx.now();
                let json = serde_json::to_string(&deleted).unwrap_or_else(|_| String::from("{}"));
                tx.gc()
                    .insert_run(&run_tx, started, finished, true, &json)?;
                note(&mut deleted, "gc_runs", tx.gc().purge_runs(keeps.2)?);
                tx.meta().set_timestamp(GC_LAST_STARTED, started)?;
                for (entity, n) in &deleted {
                    tx.stats()
                        .bump_by(Metric::GcPurged, entity, *n, started, &zone_tx)?;
                }
                Ok(Swept {
                    victims,
                    blobs,
                    known_blobs: tx.gc().blob_ids()?,
                    deleted,
                })
            })
            .await;
        let swept = match swept {
            Ok(swept) => swept,
            Err(e) => {
                // Провал виден в журнале и не повторяется каждый тик до следующего слота.
                let finished = shared.now();
                let run_row = run.clone();
                let recorded = shared
                    .db()
                    .write(move |tx| -> DbResult<()> {
                        tx.gc()
                            .insert_run(&run_row, started, finished, false, "{}")?;
                        tx.meta().set_timestamp(GC_LAST_STARTED, started)
                    })
                    .await;
                if let Err(e) = recorded {
                    log::error!(error:% = e; "failed to record a failed gc run");
                }
                return Err(e);
            }
        };

        let ids: Vec<ChatId> = swept.victims.iter().map(|v| v.id).collect();
        for chat in &ids {
            shared.access().forget(*chat);
        }
        let (blob_dir, files, known, orphan_after) = (
            cfg.blob_dir(),
            swept.blobs.clone(),
            swept.known_blobs,
            *gc.blob_orphan_after,
        );
        let files_removed = tokio::task::spawn_blocking(move || {
            remove_blob_files(&blob_dir, &files, &known, orphan_after)
        })
        .await
        .unwrap_or_else(|e| {
            log::error!(error:% = e; "blob cleanup task failed");
            0
        });
        drop(pauses);
        let mut by_surface: BTreeMap<String, (ChatRef, Vec<ChatId>)> = BTreeMap::new();
        for victim in swept.victims {
            by_surface
                .entry(victim.surface.id.as_str().to_owned())
                .or_insert_with(|| (victim.clone(), Vec::new()))
                .1
                .push(victim.id);
        }
        for (_, (victim, ids)) in by_surface {
            shared
                .bus()
                .publish(Event::ChatPurged(ChatPurged { ids }), Some(victim.surface));
        }
        if let Err(e) = shared.db().exclusive(|conn| db::gc::vacuum(conn)).await {
            log::warn!(error:% = e; "gc vacuum failed");
        }
        log::info!(run:% = run, chats = ids.len(), blobs = swept.blobs.len(), files_removed; "gc done");
        Ok(Report {
            run,
            started,
            chats: ids,
            blobs: swept.blobs,
            deleted: swept.deleted,
        })
    }

    pub async fn run(self, mut rx: SlaveRx) -> eyre::Result<()> {
        let mut ticks = tokio::time::interval(*self.kernel.shared().cfg().gc.tick);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    if let Err(e) = self.tick().await {
                        log::error!(error:% = e; "gc tick failed");
                    }
                }
                _ = rx.recv() => break,
            }
        }
        log::info!("gc stopped");
        Ok(())
    }
}

fn note(deleted: &mut BTreeMap<String, u64>, entity: &str, n: usize) {
    deleted.insert(entity.to_owned(), n as u64);
}

/// Каталоги удалённых blob'ов и сироты старше `orphan_after` (`08 §3.2`);
/// `.staging` чистит старт ядра. Возраст — по часам файловой системы.
pub fn remove_blob_files(
    blob_dir: &Path,
    deleted: &[BlobId],
    known: &[String],
    orphan_after: Duration,
) -> usize {
    let mut removed = 0;
    for id in deleted {
        match std::fs::remove_dir_all(blob_dir.join(id.as_str())) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!(blob:% = id, error:% = e; "failed to remove a blob directory"),
        }
    }
    let Ok(entries) = std::fs::read_dir(blob_dir) else {
        return removed;
    };
    let cutoff = SystemTime::now().checked_sub(orphan_after);
    let known: BTreeSet<&str> = known.iter().map(String::as_str).collect();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == STAGING || known.contains(name.as_ref()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let orphan = meta.is_dir()
            && meta
                .modified()
                .ok()
                .zip(cutoff)
                .is_some_and(|(modified, cutoff)| modified <= cutoff);
        if orphan {
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => {
                    log::warn!(dir:% = name, error:% = e; "failed to remove an orphan blob directory")
                }
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    fn ts(secs: u64) -> Timestamp {
        Timestamp::from_nanos(NonZeroU64::new(secs * 1_000_000_000).expect("non-zero"))
    }

    #[test]
    fn weekly_slot_is_due_once_and_the_gauge_moves_on() {
        let cfg = GcConfig::default();
        let zone = DayZone::Utc;
        // 2026-07-01 (Wed) 02:00Z; last Sunday 03:00 was 2026-06-28.
        let now = ts(1_782_871_200);
        let current = slot(&cfg, &zone, now);
        assert_eq!(zone.day(current), "2026-06-28");
        assert!(is_due(&cfg, &zone, None, now));
        assert!(is_due(
            &cfg,
            &zone,
            Some(ts(1_782_871_200 - 10 * 86_400)),
            now
        ));
        assert!(!is_due(&cfg, &zone, Some(current), now));
        assert_eq!(next_at(&cfg, &zone, None, now), current);
        assert_eq!(
            zone.day(next_at(&cfg, &zone, Some(current), now)),
            "2026-07-05"
        );
    }

    #[test]
    fn orphan_sweep_spares_staging_known_and_fresh_dirs() {
        let dir = tempfile::tempdir().expect("tmp");
        for name in ["known", "fresh", "old", STAGING] {
            std::fs::create_dir(dir.path().join(name)).expect("mkdir");
        }
        let past = SystemTime::now() - Duration::from_secs(3 * 86_400);
        std::fs::File::open(dir.path().join("old"))
            .and_then(|f| f.set_modified(past))
            .expect("set mtime");
        std::fs::File::open(dir.path().join(STAGING))
            .and_then(|f| f.set_modified(past))
            .expect("set mtime");
        let removed = remove_blob_files(
            dir.path(),
            &[BlobId::from("gone")],
            &[String::from("known")],
            Duration::from_secs(86_400),
        );
        assert_eq!(removed, 1, "only the old orphan");
        assert!(dir.path().join("known").exists());
        assert!(dir.path().join("fresh").exists());
        assert!(dir.path().join(STAGING).exists());
        assert!(!dir.path().join("old").exists());
    }
}
