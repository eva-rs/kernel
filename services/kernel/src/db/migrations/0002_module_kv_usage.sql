-- Вес KV модуля в квоте `modules.kv_quota` (06 §5): `kv.set` читает одну строку
-- под писателем вместо SUM по всем ключам модуля. Цена строки — как `db::kv::row_cost`.
CREATE TABLE module_kv_usage (module TEXT PRIMARY KEY, bytes INTEGER NOT NULL CHECK (bytes >= 0)) STRICT, WITHOUT ROWID;

INSERT INTO module_kv_usage (module, bytes)
  SELECT module, SUM(octet_length(key) + length(value) + 64) FROM module_kv GROUP BY module;

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
