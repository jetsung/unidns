-- unidns 初始表结构（合并自早期 0001-0004：项目无线上库，直接建最终结构）

CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    provider_type TEXT NOT NULL CHECK (provider_type IN ('aliyun', 'dnspod', 'dnspod_intl', 'cloudflare', 'edgeone', 'edgeone_intl')),
    credentials_enc BLOB NOT NULL,
    credentials_mask TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'enabled' CHECK (status IN ('enabled', 'disabled')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_verified_at TEXT,
    last_verify_ok INTEGER,
    last_verify_msg TEXT
);

CREATE TABLE IF NOT EXISTS domains (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    provider_domain_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT '',
    synced_at TEXT NOT NULL,
    remark TEXT NOT NULL DEFAULT '',
    UNIQUE (account_id, name)
);

CREATE TABLE IF NOT EXISTS records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    domain_id INTEGER NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    provider_record_id TEXT NOT NULL,
    host TEXT NOT NULL,
    rtype TEXT NOT NULL,
    value TEXT NOT NULL,
    ttl INTEGER NOT NULL,
    line TEXT NOT NULL DEFAULT 'default',
    proxied INTEGER,
    status TEXT NOT NULL DEFAULT 'enabled',
    pending_sync INTEGER NOT NULL DEFAULT 0,
    synced_at TEXT NOT NULL,
    remark TEXT NOT NULL DEFAULT '',
    UNIQUE (domain_id, provider_record_id)
);

CREATE TABLE IF NOT EXISTS sync_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER REFERENCES accounts(id) ON DELETE CASCADE,
    scope TEXT NOT NULL CHECK (scope IN ('account', 'all')),
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL CHECK (status IN ('running', 'success', 'failed')),
    error TEXT,
    stats_json TEXT
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_domains_account ON domains(account_id);
CREATE INDEX IF NOT EXISTS idx_records_domain ON records(domain_id);
CREATE INDEX IF NOT EXISTS idx_records_account ON records(account_id);
CREATE INDEX IF NOT EXISTS idx_sync_runs_account ON sync_runs(account_id);
