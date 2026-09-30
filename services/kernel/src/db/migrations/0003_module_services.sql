-- Реестр сервисов (06 §2.1): ядро само набирает модуль, поэтому ключ PSK хранится
-- как есть — хэш не годится; module_services и бэкапы БД — секрет.
CREATE TABLE module_services (module TEXT PRIMARY KEY,
  address TEXT NOT NULL,
  key TEXT NOT NULL CHECK (key <> ''),
  added_via TEXT NOT NULL,
  added_at INTEGER NOT NULL) STRICT;

-- Токены только у клиентов: сервис заводится в modules.services или modules.add.
UPDATE module_tokens SET revoked_at = CAST(unixepoch('subsec') * 1000000000 AS INTEGER)
  WHERE kind = 'service' AND revoked_at IS NULL;
ALTER TABLE module_tokens DROP COLUMN kind;
