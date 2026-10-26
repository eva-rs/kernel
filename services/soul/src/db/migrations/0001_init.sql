-- 0001: схема 10 §2 целиком. Правка относительно документа одна: триггеры
-- chat_speech_ai/_ad/_au и module_kv_usage_ai/_au/_ad, заданные там
-- комментарием, выписаны явно.

-- 2.1 Транскрипт (01)
CREATE TABLE segments (
  id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER,
  reason TEXT NOT NULL CHECK (reason IN ('initial','compact','downshift','overflow','manual','reset')),
  summary TEXT, verbatim_tail TEXT,   -- источник первого user-сообщения сегмента (01 §6.2), не system; NULL при 'initial' и голом 'reset' чата на surface system (08 §2.2)
  summary_model TEXT, summary_path TEXT CHECK (summary_path IN ('warm','cold','bisect','fallback')), summary_attempts INTEGER NOT NULL DEFAULT 0,   -- провенанс компакции (01 §6.2 п.6)
  failed_compactions INTEGER NOT NULL DEFAULT 0,   -- CompactionFailed подряд; второй даёт аварийный summary (01 §6.2 п.4)
  system_snapshot BLOB NOT NULL, tools_snapshot BLOB NOT NULL, hot_memory_ids BLOB NOT NULL, UNIQUE (chat_id, seq)) STRICT;
CREATE UNIQUE INDEX segments_open ON segments(chat_id) WHERE ended_at IS NULL;      -- текущий сегмент чата

CREATE TABLE events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
  segment_id INTEGER NOT NULL REFERENCES segments(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('message','thinking','tool_use','tool_result','system_text','note_updated',
    'memory_updated','tools_added','tools_removed','reminder','params_changed','segment_reset','turn_ended')),
  payload BLOB NOT NULL, turn_id TEXT, step INTEGER, created_at INTEGER NOT NULL, UNIQUE (segment_id, seq)) STRICT;
CREATE INDEX events_turn ON events(turn_id) WHERE turn_id IS NOT NULL;
CREATE TRIGGER events_append_only BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT,'events are append-only'); END;

CREATE TABLE messages (   -- изменяемая сторона речи (05 §1.6) только для поиска/атрибуции: sender после merge, external_id после доставки, indexed_at; рендер читает events.payload (01 §2.3)
  chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  event_id INTEGER NOT NULL UNIQUE REFERENCES events(id) ON DELETE CASCADE, role TEXT NOT NULL CHECK (role IN ('user','assistant')),
  sender_person_id TEXT REFERENCES persons(id) ON DELETE RESTRICT,                  -- NULL = unknown, никогда не master; текст soul (01 §6.2, 05 §6.1) = 'p0'
  external_id TEXT, reply_to_external_id TEXT, turn_id TEXT, indexed_at INTEGER,    -- turn_id NULL = silent
  PRIMARY KEY (chat_id, seq)) STRICT;
CREATE INDEX messages_external ON messages(chat_id, external_id) WHERE external_id IS NOT NULL;
CREATE INDEX messages_unindexed ON messages(chat_id) WHERE indexed_at IS NULL;
CREATE INDEX messages_sender ON messages(sender_person_id);

CREATE TABLE tool_results (
  chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE, tseq INTEGER NOT NULL,
  event_id INTEGER NOT NULL UNIQUE REFERENCES events(id) ON DELETE CASCADE,
  segment_id INTEGER NOT NULL REFERENCES segments(id) ON DELETE CASCADE,
  tool_id INTEGER REFERENCES tools(tool_id),                                          -- NULL = extra_tools тура (02 §4.4, 06 §5): turn_ended.by = tool{None, canonical}, строки tool_stats нет
  dedup_key BLOB NOT NULL, dedup_of INTEGER, content BLOB, created_at INTEGER NOT NULL,   -- content: полные байты; окно для модели — в events.payload (02 §1.1)
  synthetic TEXT CHECK (synthetic IN ('not_started','interrupted','cancelled','tool_unavailable')),   -- закрытие висячего tool_use самим soul (01 §1.2): is_error, вне дедупа
  PRIMARY KEY (chat_id, tseq), CHECK ((dedup_of IS NULL) = (content IS NOT NULL)), CHECK (synthetic IS NULL OR dedup_of IS NULL)) STRICT;
CREATE INDEX tool_results_dedup ON tool_results(chat_id, segment_id, dedup_key) WHERE dedup_of IS NULL AND synthetic IS NULL;

CREATE TABLE token_ratios (model TEXT PRIMARY KEY, bytes_per_token REAL NOT NULL, samples INTEGER NOT NULL, updated_at INTEGER NOT NULL) STRICT;

-- 2.2 Инструменты (02)
CREATE TABLE tools (
  tool_id INTEGER PRIMARY KEY AUTOINCREMENT, module TEXT NOT NULL, path TEXT NOT NULL, canonical TEXT NOT NULL,  -- module 'soul' для своих
  decl TEXT NOT NULL, spec_hash BLOB NOT NULL, registered_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
  retired_at INTEGER, online INTEGER NOT NULL DEFAULT 0 CHECK (online IN (0,1)), UNIQUE (module, path)) STRICT;
CREATE INDEX tools_canonical ON tools(canonical) WHERE retired_at IS NULL;          -- коллизия: меньший tool_id побеждает

CREATE TABLE tool_groups (path TEXT PRIMARY KEY, about TEXT NOT NULL DEFAULT '', source TEXT NOT NULL CHECK (source IN ('module','sleep','master')), updated_at INTEGER NOT NULL) STRICT;
CREATE TABLE tool_placements (tool_id INTEGER PRIMARY KEY REFERENCES tools(tool_id) ON DELETE CASCADE,
  path TEXT NOT NULL REFERENCES tool_groups(path) ON DELETE RESTRICT,
  source TEXT NOT NULL CHECK (source IN ('sleep','master')), updated_at INTEGER NOT NULL) STRICT;
CREATE TABLE tool_overrides (tool_id INTEGER PRIMARY KEY REFERENCES tools(tool_id) ON DELETE CASCADE,
  master_only INTEGER CHECK (master_only IN (0,1)), trusted_only INTEGER CHECK (trusted_only IN (0,1)),
  gate TEXT CHECK (gate IN ('forbid')), attention TEXT CHECK (attention IN ('always','reserved')),
  note TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL) STRICT;
CREATE TABLE tool_pins (chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE, tool_id INTEGER NOT NULL REFERENCES tools(tool_id) ON DELETE CASCADE, until INTEGER NOT NULL, PRIMARY KEY (chat_id, tool_id)) STRICT;
CREATE TABLE chat_warm_sets (chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE, day TEXT NOT NULL,
  tool_ids TEXT NOT NULL, hidden_count INTEGER NOT NULL, over_budget INTEGER NOT NULL CHECK (over_budget IN (0,1)),
  computed_at INTEGER NOT NULL, PRIMARY KEY (chat_id, day)) STRICT;
CREATE TABLE chat_loaded_tools (   -- `loaded` чата: tools_load до ночного пересчёта, переживает компакцию и рестарт
  chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
  tool_id INTEGER NOT NULL REFERENCES tools(tool_id) ON DELETE CASCADE, loaded_at INTEGER NOT NULL, PRIMARY KEY (chat_id, tool_id)) STRICT;

-- 2.3 Память (03)
CREATE TABLE memory (
  id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL CHECK (kind IN ('fact','event','core','procedure')),
  text TEXT NOT NULL, name TEXT, description TEXT, tags TEXT,
  visibility TEXT NOT NULL CHECK (visibility IN ('public','trusted','private')),
  importance REAL NOT NULL DEFAULT 0.5 CHECK (importance BETWEEN 0 AND 1), confidence REAL NOT NULL DEFAULT 1.0 CHECK (confidence BETWEEN 0 AND 1),
  source_actor TEXT REFERENCES persons(id) ON DELETE RESTRICT, source_chat INTEGER, source_surface TEXT,
  source_trusted_chat INTEGER CHECK (source_trusted_chat IN (0,1)), source_run_id TEXT,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, consolidated_at INTEGER, occurred_at INTEGER,
  valid_from INTEGER NOT NULL, valid_to INTEGER, supersedes INTEGER REFERENCES memory(id) ON DELETE SET NULL,
  version INTEGER NOT NULL DEFAULT 1, last_reinforced_at INTEGER NOT NULL, access_count REAL NOT NULL DEFAULT 0,
  embedding BLOB, embed_model TEXT) STRICT;
CREATE INDEX memory_current ON memory(kind, visibility) WHERE valid_to IS NULL;
CREATE INDEX memory_event_time ON memory(occurred_at) WHERE valid_to IS NULL AND kind = 'event';
CREATE INDEX memory_chain ON memory(supersedes);
CREATE INDEX memory_embed ON memory(embed_model) WHERE valid_to IS NULL;           -- очередь backfill
CREATE UNIQUE INDEX memory_procedure_name ON memory(name) WHERE valid_to IS NULL AND kind = 'procedure';

CREATE VIRTUAL TABLE memory_fts USING fts5(text, name, description, tags, content='memory', content_rowid='id', tokenize='trigram case_sensitive 0');
CREATE TRIGGER memory_ai AFTER INSERT ON memory BEGIN INSERT INTO memory_fts(rowid,text,name,description,tags) VALUES (new.id,new.text,new.name,new.description,new.tags); END;
CREATE TRIGGER memory_ad AFTER DELETE ON memory BEGIN INSERT INTO memory_fts(memory_fts,rowid,text,name,description,tags) VALUES ('delete',old.id,old.text,old.name,old.description,old.tags); END;
CREATE TRIGGER memory_au AFTER UPDATE OF text,name,description,tags ON memory BEGIN
  INSERT INTO memory_fts(memory_fts,rowid,text,name,description,tags) VALUES ('delete',old.id,old.text,old.name,old.description,old.tags);
  INSERT INTO memory_fts(rowid,text,name,description,tags) VALUES (new.id,new.text,new.name,new.description,new.tags); END;

CREATE TABLE memory_archive (   -- повторный forget после restore — UPSERT той же строки (restored_at := NULL)
  memory_id INTEGER PRIMARY KEY REFERENCES memory(id) ON DELETE CASCADE,
  reason TEXT NOT NULL CHECK (reason IN ('forget','merge','split','demote')), why TEXT NOT NULL,
  run_id TEXT, actor TEXT REFERENCES persons(id) ON DELETE RESTRICT, archived_at INTEGER NOT NULL, restored_at INTEGER) STRICT;
CREATE INDEX memory_archive_gc ON memory_archive(archived_at) WHERE restored_at IS NULL;
CREATE TABLE memory_link (from_id INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE, to_id INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  rel TEXT NOT NULL CHECK (rel IN ('split_from','contradicts','refines')), created_at INTEGER NOT NULL, PRIMARY KEY (from_id, to_id, rel)) STRICT;
CREATE TABLE embed_cache (   -- «kv компонента» из 03 §8: blake3(text) → вектор
  text_hash BLOB NOT NULL, embed_model TEXT NOT NULL, embedding BLOB NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (text_hash, embed_model)) STRICT;

-- 2.4 Заметки (04)
CREATE TABLE chat_notes (
  chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
  name TEXT NOT NULL CHECK (name NOT GLOB '*[^a-z0-9_-]*' AND length(name) BETWEEN 1 AND 48),
  description TEXT NOT NULL CHECK (length(description) BETWEEN 1 AND 120), body TEXT NOT NULL CHECK (body <> ''), prev_body TEXT,
  pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0,1)), created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER,
  PRIMARY KEY (chat_id, name)) STRICT;
CREATE INDEX chat_notes_gc ON chat_notes(deleted_at) WHERE deleted_at IS NOT NULL;
CREATE TABLE notes_snapshots (
  id INTEGER PRIMARY KEY, chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL, reason TEXT NOT NULL, count INTEGER NOT NULL CHECK (count >= 0)) STRICT;
CREATE INDEX notes_snapshots_chat ON notes_snapshots(chat_id, id DESC);
CREATE TABLE notes_snapshot_entries (
  snapshot_id INTEGER NOT NULL REFERENCES notes_snapshots(id) ON DELETE CASCADE,
  name TEXT NOT NULL, description TEXT NOT NULL, body TEXT NOT NULL, prev_body TEXT,
  pinned INTEGER NOT NULL CHECK (pinned IN (0,1)), created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY (snapshot_id, name)) STRICT;
CREATE TABLE notes_state (chat_id INTEGER PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE, mode TEXT NOT NULL CHECK (mode IN ('inline','squeeze','index')), since_at INTEGER NOT NULL, squeeze_turns INTEGER NOT NULL DEFAULT 0,
  wrap_up_segment INTEGER REFERENCES segments(id) ON DELETE SET NULL, nudge_segment INTEGER REFERENCES segments(id) ON DELETE SET NULL,
  seen_at INTEGER) STRICT;

-- 2.5 Чаты, личности (05)
CREATE TABLE surfaces (
  id TEXT PRIMARY KEY CHECK (id GLOB '[a-z]*' AND id NOT GLOB '*[^a-z0-9_]*' AND length(id) <= 32),
  name TEXT NOT NULL, module TEXT REFERENCES modules(name), rules TEXT, trusted INTEGER CHECK (trusted IN (0,1)),   -- module = создатель (surfaces.create, 06 §2); NULL у builtin local/system, засеянных самим soul при первом старте
  model TEXT, effort INTEGER CHECK (effort BETWEEN 1 AND 4), thinking TEXT CHECK (thinking IN ('adaptive','off')),
  compact_at REAL CHECK (compact_at > 0 AND compact_at <= 1), builtin TEXT CHECK (builtin IN ('local','system')), created_at INTEGER NOT NULL) STRICT;
CREATE TABLE folders (
  id INTEGER PRIMARY KEY AUTOINCREMENT, surface_id TEXT NOT NULL REFERENCES surfaces(id) ON DELETE RESTRICT, name TEXT NOT NULL,
  builtin INTEGER NOT NULL DEFAULT 0 CHECK (builtin IN (0,1)), trusted INTEGER CHECK (trusted IN (0,1)),
  model TEXT, effort INTEGER CHECK (effort BETWEEN 1 AND 4), thinking TEXT CHECK (thinking IN ('adaptive','off')),
  compact_at REAL CHECK (compact_at > 0 AND compact_at <= 1), UNIQUE (surface_id, name)) STRICT;
CREATE TABLE chats (
  id INTEGER PRIMARY KEY AUTOINCREMENT, folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE RESTRICT,  -- удаление папки = перенос в Unsorted
  kind TEXT NOT NULL CHECK (kind IN ('ephemeral','persistent')), title TEXT, external_title TEXT,
  pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0,1)), alerted INTEGER NOT NULL DEFAULT 0 CHECK (alerted IN (0,1)),
  undeletable INTEGER NOT NULL DEFAULT 0 CHECK (undeletable IN (0,1)), deleted_at INTEGER,
  model TEXT, effort INTEGER CHECK (effort BETWEEN 1 AND 4), thinking TEXT CHECK (thinking IN ('adaptive','off')),
  trusted INTEGER CHECK (trusted IN (0,1)), compact_at REAL CHECK (compact_at > 0 AND compact_at <= 1),
  project TEXT, workdir TEXT, workdir_on TEXT,                                                                    -- имя endpoint'а рук в hands.endpoints (07 §3), без CHECK: набор задан конфигом; NULL = hands.default_endpoint
  tool_seq INTEGER NOT NULL DEFAULT 0, token_anchor_event_id INTEGER, token_anchor_tokens INTEGER,               -- 01 §9
  request_gap_ema REAL, last_request_at INTEGER,                                                                 -- 01 §7; EMA в наносекундах (§1)
  reset_pending TEXT CHECK (reset_pending IN ('reset')),                                                          -- 05 §2.3, §3 п.5: отложенный SegmentReset в живом туре, снимается turns на границе шага (01 §6.1); NULL = нет
  created_at INTEGER NOT NULL, last_activity_at INTEGER NOT NULL,
  CHECK (NOT (kind = 'ephemeral' AND (pinned = 1 OR undeletable = 1)))) STRICT;
CREATE INDEX chats_folder ON chats(folder_id, deleted_at, last_activity_at DESC);
CREATE INDEX chats_ephemeral ON chats(last_activity_at) WHERE kind = 'ephemeral';
CREATE INDEX chats_deleted ON chats(deleted_at) WHERE deleted_at IS NOT NULL;
CREATE TRIGGER chats_kind_monotonic BEFORE UPDATE OF kind ON chats WHEN old.kind = 'persistent' AND new.kind = 'ephemeral'
  BEGIN SELECT RAISE(ABORT,'kind is monotonic'); END;
CREATE TRIGGER chats_undeletable_mark BEFORE UPDATE OF deleted_at ON chats WHEN new.deleted_at IS NOT NULL AND new.undeletable = 1
  BEGIN SELECT RAISE(ABORT,'undeletable'); END;
CREATE TRIGGER chats_undeletable_purge BEFORE DELETE ON chats WHEN old.undeletable = 1 BEGIN SELECT RAISE(ABORT,'undeletable'); END;

CREATE TABLE chat_aliases (surface_id TEXT NOT NULL REFERENCES surfaces(id) ON DELETE RESTRICT, external_id TEXT NOT NULL,
  chat_id INTEGER NOT NULL UNIQUE REFERENCES chats(id) ON DELETE CASCADE, external_kind TEXT NOT NULL CHECK (external_kind IN ('direct','group','channel')), created_at INTEGER NOT NULL,
  PRIMARY KEY (surface_id, external_id)) STRICT;
CREATE TABLE persons (id TEXT PRIMARY KEY CHECK (id GLOB 'p[0-9]*'), name TEXT NOT NULL, master INTEGER NOT NULL DEFAULT 0 CHECK (master IN (0,1)), whitelisted INTEGER NOT NULL DEFAULT 0 CHECK (whitelisted IN (0,1)),
  created_at INTEGER NOT NULL) STRICT;
CREATE TABLE identities (person_id TEXT NOT NULL REFERENCES persons(id) ON DELETE CASCADE, surface_id TEXT NOT NULL REFERENCES surfaces(id) ON DELETE RESTRICT, external_id TEXT NOT NULL,
  handle TEXT, name TEXT, first_seen INTEGER NOT NULL, PRIMARY KEY (surface_id, external_id)) STRICT;
CREATE INDEX identities_person ON identities(person_id);

CREATE TABLE chat_speech (   -- чанки речи и строка названия: зеркало для FTS, ключ для векторов
  id INTEGER PRIMARY KEY, chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE, seq INTEGER, chunk_no INTEGER NOT NULL DEFAULT 0,
  kind TEXT NOT NULL CHECK (kind IN ('speech','title')), text TEXT NOT NULL, CHECK ((kind = 'title') = (seq IS NULL)),
  FOREIGN KEY (chat_id, seq) REFERENCES messages(chat_id, seq) ON DELETE CASCADE) STRICT;
CREATE UNIQUE INDEX chat_speech_chunk ON chat_speech(chat_id, seq, chunk_no) WHERE kind = 'speech';
CREATE UNIQUE INDEX chat_speech_title ON chat_speech(chat_id) WHERE kind = 'title';
CREATE VIRTUAL TABLE chat_speech_fts USING fts5(text, content='chat_speech', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER chat_speech_ai AFTER INSERT ON chat_speech BEGIN INSERT INTO chat_speech_fts(rowid,text) VALUES (new.id,new.text); END;
CREATE TRIGGER chat_speech_ad AFTER DELETE ON chat_speech BEGIN INSERT INTO chat_speech_fts(chat_speech_fts,rowid,text) VALUES ('delete',old.id,old.text); END;
CREATE TRIGGER chat_speech_au AFTER UPDATE OF text ON chat_speech BEGIN
  INSERT INTO chat_speech_fts(chat_speech_fts,rowid,text) VALUES ('delete',old.id,old.text);
  INSERT INTO chat_speech_fts(rowid,text) VALUES (new.id,new.text); END;
CREATE TABLE chat_speech_vec (chunk_id INTEGER PRIMARY KEY REFERENCES chat_speech(id) ON DELETE CASCADE, embedding BLOB NOT NULL, embed_model TEXT NOT NULL) STRICT;

-- 2.6 Модули (06)
CREATE TABLE modules (name TEXT PRIMARY KEY, state TEXT NOT NULL CHECK (state IN ('connected','offline','forgotten')),
  version TEXT NOT NULL, proto_version TEXT NOT NULL, capabilities BLOB NOT NULL,   -- capability hands не существует (руки — библиотека eva-hands в soul, 07 §3); provider — запасная дверь, принимается только если модуль назван в llm.providers с kind: module (06 §3)
  first_seen_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL) STRICT;
CREATE TABLE module_services (module TEXT PRIMARY KEY,   -- soul сам набирает сервис, поэтому ключ PSK хранится как есть — хэш не годится; таблица и бэкапы БД — секрет
  address TEXT NOT NULL,
  key TEXT NOT NULL CHECK (key <> ''),
  added_via TEXT NOT NULL,
  added_at INTEGER NOT NULL) STRICT;
CREATE TABLE module_tokens (token_hash BLOB PRIMARY KEY, module TEXT NOT NULL,   -- токен клиента; module без FK: токен выдаётся до первого hello
  issued_by TEXT REFERENCES persons(id) ON DELETE RESTRICT,                      -- путь тула: актор тура
  issued_via TEXT,                                                               -- путь операции modules.token_issue: caller.module() выдавшего Client
  issued_at INTEGER NOT NULL, revoked_at INTEGER, CHECK (issued_by IS NOT NULL OR issued_via IS NOT NULL)) STRICT;
CREATE INDEX module_tokens_live ON module_tokens(module) WHERE revoked_at IS NULL;
CREATE TABLE module_kv (module TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (module, key)) STRICT;
-- Вес KV модуля в квоте modules.kv_quota (06 §5): kv.set читает одну строку под писателем вместо SUM по ключам модуля. Цена строки — как db::kv::row_cost.
CREATE TABLE module_kv_usage (module TEXT PRIMARY KEY, bytes INTEGER NOT NULL CHECK (bytes >= 0)) STRICT, WITHOUT ROWID;
CREATE TRIGGER module_kv_usage_ai AFTER INSERT ON module_kv BEGIN
  INSERT INTO module_kv_usage (module, bytes) VALUES (new.module, octet_length(new.key) + length(new.value) + 64)
    ON CONFLICT (module) DO UPDATE SET bytes = bytes + excluded.bytes;
END;
CREATE TRIGGER module_kv_usage_au AFTER UPDATE ON module_kv BEGIN
  UPDATE module_kv_usage SET bytes = bytes - (octet_length(old.key) + length(old.value) + 64) WHERE module = old.module;
  INSERT INTO module_kv_usage (module, bytes) VALUES (new.module, octet_length(new.key) + length(new.value) + 64)
    ON CONFLICT (module) DO UPDATE SET bytes = bytes + excluded.bytes;
END;
CREATE TRIGGER module_kv_usage_ad AFTER DELETE ON module_kv BEGIN
  UPDATE module_kv_usage SET bytes = bytes - (octet_length(old.key) + length(old.value) + 64) WHERE module = old.module;
END;

-- 2.7 Рабочие туры (07)
CREATE TABLE agents (id INTEGER PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,
  parent_chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,          -- единственное место parent-связи
  name TEXT NOT NULL CHECK (name NOT GLOB '*[^a-z0-9_-]*' AND length(name) BETWEEN 1 AND 32),   -- slug (07 §2.1): в reminder/[team]-маркерах только через sanitize_inline
  task TEXT NOT NULL, tools TEXT NOT NULL, report_schema TEXT,                       -- tools: JSON [tool_id], пути резолвятся при spawn (07 §2.1)
  master_turn INTEGER NOT NULL CHECK (master_turn IN (0,1)),
  status TEXT NOT NULL CHECK (status IN ('running','idle','done','failed','stopped')), started_at INTEGER NOT NULL, finished_at INTEGER) STRICT;
CREATE INDEX agents_parent ON agents(parent_chat_id, status);
CREATE UNIQUE INDEX agents_live_name ON agents(parent_chat_id, name) WHERE status IN ('running','idle');
CREATE TABLE agent_messages (id INTEGER PRIMARY KEY AUTOINCREMENT, agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE, direction TEXT NOT NULL CHECK (direction IN ('to_agent','to_parent')), body TEXT NOT NULL, created_at INTEGER NOT NULL, read_at INTEGER) STRICT;
CREATE INDEX agent_messages_unread ON agent_messages(agent_id, direction) WHERE read_at IS NULL;
CREATE TABLE pending (id TEXT PRIMARY KEY, chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
  turn_id TEXT NOT NULL, call_id TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('ask','permission')), payload TEXT NOT NULL,
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, resolved_at INTEGER,
  outcome TEXT CHECK (outcome IN ('answered','text_instead','late_answer','superseded','allowed','denied','expired','cancelled')),   -- late_answer/superseded: ask, переживший ask_ttl (07 §4.1)
  CHECK ((resolved_at IS NULL) = (outcome IS NULL))) STRICT;
CREATE INDEX pending_open ON pending(chat_id, expires_at) WHERE resolved_at IS NULL;
CREATE TABLE hands_grants (id INTEGER PRIMARY KEY AUTOINCREMENT,
  endpoint TEXT NOT NULL,                                                            -- имя в hands.endpoints, без FK; матч с workdir_on ?? hands.default_endpoint на момент вызова
  surface TEXT NOT NULL REFERENCES surfaces(id) ON DELETE RESTRICT,
  chat_id INTEGER REFERENCES chats(id) ON DELETE CASCADE, kind TEXT NOT NULL CHECK (kind IN ('read','write','shell')), prefix TEXT NOT NULL,
  issued_by TEXT NOT NULL REFERENCES persons(id) ON DELETE RESTRICT,                 -- только master (07 §4.2); chat_id NULL = все чаты surface на этом endpoint
  created_at INTEGER NOT NULL) STRICT;
CREATE INDEX hands_grants_match ON hands_grants(endpoint, surface, kind);
CREATE TABLE chat_plan (chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE, task_id INTEGER NOT NULL,
  position INTEGER NOT NULL, phase TEXT, title TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending','in_progress','completed','abandoned')), outcome TEXT, updated_at INTEGER NOT NULL,
  PRIMARY KEY (chat_id, task_id)) STRICT;
CREATE INDEX chat_plan_active ON chat_plan(chat_id) WHERE status = 'in_progress';   -- активных ≤ 1 + живых подагентов (07 §5): держит normalize, не схема
CREATE TABLE blobs (id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('file','image')),
  chat_id INTEGER REFERENCES chats(id) ON DELETE SET NULL,                            -- blob живёт до expires_at
  name TEXT NOT NULL CHECK (name NOT GLOB '*[/\]*' AND name NOT IN ('.','..')), media_type TEXT NOT NULL, size INTEGER NOT NULL, hash BLOB NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('upload','tool','generated')), model TEXT, prompt TEXT, cost REAL,
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL) STRICT;
CREATE INDEX blobs_expiry ON blobs(expires_at);

-- 2.8 Крон, сон, GC (08)
CREATE TABLE cron_jobs (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, task TEXT NOT NULL,
  interval INTEGER, next_fire_at INTEGER, expires_at INTEGER,          -- interval нс, next_fire_at Timestamp (§1); interval NULL = one-shot; next_fire_at NULL = one-shot запущен
  fire_pending TEXT CHECK (fire_pending IN ('keep','shift')),         -- cron_fire переживает рестарт (D9, 08 §1.4): due, пока не NULL; 'keep' не трогает слот
  model TEXT, effort INTEGER CHECK (effort BETWEEN 1 AND 4), tools TEXT, -- tools: JSON [tool_id]; NULL = без ограничений (I8)
  chat_id INTEGER NOT NULL UNIQUE REFERENCES chats(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL, runs_total INTEGER NOT NULL DEFAULT 0, spent_total REAL NOT NULL DEFAULT 0) STRICT;
CREATE INDEX cron_jobs_due ON cron_jobs(next_fire_at) WHERE next_fire_at IS NOT NULL OR fire_pending IS NOT NULL;
CREATE TABLE cron_runs (run_id TEXT PRIMARY KEY, job_id TEXT NOT NULL, started INTEGER NOT NULL, finished INTEGER, ok INTEGER CHECK (ok IN (0,1)), output TEXT, cost REAL,
  due INTEGER) STRICT;   -- due: плановый слот (next_fire_at до сдвига), NULL у запуска по cron_fire; started - due = cron_fire_lag_last (08 §4)
CREATE INDEX cron_runs_job ON cron_runs(job_id, started DESC);   -- job_id без FK: переживает удаление one-shot
CREATE TABLE sleep_runs (run_id TEXT PRIMARY KEY, started INTEGER NOT NULL, finished INTEGER, ok INTEGER CHECK (ok IN (0,1)),
  dry_run INTEGER NOT NULL CHECK (dry_run IN (0,1)), interrupted INTEGER NOT NULL DEFAULT 0 CHECK (interrupted IN (0,1)), report TEXT) STRICT;
CREATE TABLE sleep_phase_runs (run_id TEXT NOT NULL REFERENCES sleep_runs(run_id) ON DELETE CASCADE, phase TEXT NOT NULL CHECK (phase IN ('memory','groups','warm')), started INTEGER NOT NULL, finished INTEGER, model TEXT,
  ops_json TEXT NOT NULL DEFAULT '[]', applied INTEGER NOT NULL DEFAULT 0, discarded INTEGER NOT NULL DEFAULT 0, report TEXT,
  PRIMARY KEY (run_id, phase)) STRICT;
CREATE TABLE gc_runs (run_id TEXT PRIMARY KEY, started INTEGER NOT NULL, finished INTEGER, ok INTEGER CHECK (ok IN (0,1)), deleted_json TEXT) STRICT;
CREATE TABLE soul_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;

-- 2.9 Статистика (09)
CREATE TABLE tool_stats (day TEXT NOT NULL, chat_id INTEGER NOT NULL, tool_id INTEGER NOT NULL,   -- без FK: переживают purge до stats.keep
  calls INTEGER NOT NULL DEFAULT 0, turns_with_call INTEGER NOT NULL DEFAULT 0, loads INTEGER NOT NULL DEFAULT 0,
  errors INTEGER NOT NULL DEFAULT 0, ends_turn INTEGER NOT NULL DEFAULT 0, last_at INTEGER NOT NULL, PRIMARY KEY (day, chat_id, tool_id)) STRICT;
CREATE INDEX tool_stats_chat ON tool_stats(chat_id, day);
CREATE TABLE chat_stats (day TEXT NOT NULL, chat_id INTEGER NOT NULL, turns INTEGER NOT NULL DEFAULT 0, steps INTEGER NOT NULL DEFAULT 0,
  user_messages INTEGER NOT NULL DEFAULT 0, assistant_messages INTEGER NOT NULL DEFAULT 0, cancelled INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, chat_id)) STRICT;
CREATE TABLE spend (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, model TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('surface','cron','sleep','agent','system','module')),
  op TEXT NOT NULL CHECK (op IN ('step','compaction','title','embed','keep_alive','vet','other')),
  module TEXT, chat_id INTEGER, turn_id TEXT, run_id TEXT,
  input INTEGER NOT NULL, cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL, cache_write_long INTEGER NOT NULL DEFAULT 0,   -- cache_write = запись любого TTL (usage.cache_write_short + cache_write_long, 06 §6); cache_write_long ⊆ cache_write, тариф 2× (09 §2.1)
  output INTEGER NOT NULL, reasoning INTEGER NOT NULL DEFAULT 0,
  context_edits_applied INTEGER NOT NULL DEFAULT 0,                                    -- 01 §5
  cache_break_reason TEXT CHECK (cache_break_reason IN ('model_change','tools_change','segment_reset','ttl_expired','context_edit','thinking_change','effort_change','first_request','unknown')),   -- effort_change: смена effort под fallback без effort_in_messages (01 §3)
  cost REAL, currency TEXT, cost_source TEXT NOT NULL CHECK (cost_source IN ('provider','pricebook','none')),
  CHECK ((cost IS NULL) = (currency IS NULL)), CHECK ((source = 'module') = (module IS NOT NULL))) STRICT;
CREATE INDEX spend_chat ON spend(chat_id, ts); CREATE INDEX spend_ts ON spend(ts); CREATE INDEX spend_run ON spend(run_id) WHERE run_id IS NOT NULL;
CREATE INDEX spend_turn ON spend(turn_id) WHERE turn_id IS NOT NULL;
CREATE TABLE health_stats (day TEXT NOT NULL, metric TEXT NOT NULL, key TEXT NOT NULL DEFAULT '', count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, metric, key)) STRICT;

-- 2.10 Служебное
CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum BLOB NOT NULL, applied_at INTEGER NOT NULL) STRICT;
