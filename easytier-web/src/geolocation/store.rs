use std::{net::IpAddr, path::Path, time::Duration};

use anyhow::{Context as _, Result, bail};
use sqlx::{
    Row as _, Sqlite, SqlitePool, Transaction,
    sqlite::{
        SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow,
        SqliteSynchronous,
    },
};

use super::{CachedCity, CityLocation};

const LEASE_SECONDS: i64 = 120;
const DAY_SECONDS: i64 = 86_400;

#[derive(Clone, Debug)]
pub(super) struct Store {
    pool: SqlitePool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProviderPermit {
    Granted,
    WaitUntil(i64),
}

fn ip_key(ip: IpAddr) -> String {
    ip.to_canonical().to_string()
}

fn cached_city(row: Option<SqliteRow>) -> Result<Option<CachedCity>> {
    let Some(row) = row else {
        return Ok(None);
    };
    let Some(location) = row.try_get::<Option<String>, _>("location_json")? else {
        return Ok(None);
    };
    Ok(Some(CachedCity {
        location: serde_json::from_str(&location).context("invalid cached city location")?,
        source: row
            .try_get::<Option<String>, _>("source")?
            .context("cached city has no source")?,
        updated_at: row
            .try_get::<Option<i64>, _>("updated_at")?
            .context("cached city has no update time")?,
    }))
}

impl Store {
    pub(super) async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file_options = tokio::fs::OpenOptions::new();
        file_options.create(true).append(true);
        #[cfg(unix)]
        file_options.mode(0o600);
        let file = file_options
            .open(path)
            .await
            .with_context(|| format!("open city cache {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        drop(file);

        let options = Self::connect_options()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let store = Self::connect(options, 4).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for suffix in ["-wal", "-shm"] {
                let mut sidecar = path.as_os_str().to_os_string();
                sidecar.push(suffix);
                match tokio::fs::set_permissions(
                    Path::new(&sidecar),
                    std::fs::Permissions::from_mode(0o600),
                )
                .await
                {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(store)
    }

    fn connect_options() -> SqliteConnectOptions {
        SqliteConnectOptions::new()
            .busy_timeout(Duration::from_secs(5))
            .auto_vacuum(SqliteAutoVacuum::Incremental)
            .pragma("secure_delete", "ON")
            .synchronous(SqliteSynchronous::Normal)
    }

    #[cfg(test)]
    pub(super) async fn memory() -> Result<Self> {
        Self::connect(Self::connect_options().in_memory(true), 1).await
    }

    #[cfg(test)]
    pub(super) fn test_pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn connect(options: SqliteConnectOptions, max_connections: u32) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *transaction)
            .await?;
        if version != 0 && version != 1 {
            transaction.rollback().await?;
            pool.close().await;
            bail!("unsupported city cache schema version {version}");
        }
        if version == 0 {
            let tables: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .fetch_one(&mut *transaction)
            .await?;
            if tables != 0 {
                transaction.rollback().await?;
                pool.close().await;
                bail!("city cache file already contains a different database schema");
            }
            sqlx::raw_sql(
                r#"
                CREATE TABLE city_cache (
                    ip TEXT PRIMARY KEY NOT NULL,
                    location_json TEXT,
                    source TEXT,
                    updated_at INTEGER,
                    last_used INTEGER NOT NULL,
                    last_attempt INTEGER,
                    refresh_at INTEGER NOT NULL DEFAULT 0,
                    lease_until INTEGER,
                    fail_count INTEGER NOT NULL DEFAULT 0,
                    CHECK (
                        (location_json IS NULL AND source IS NULL AND updated_at IS NULL)
                        OR
                        (location_json IS NOT NULL AND source IS NOT NULL AND updated_at IS NOT NULL)
                    )
                );
                CREATE INDEX city_cache_lru ON city_cache(last_used, ip);
                CREATE INDEX city_cache_due ON city_cache(refresh_at, last_used);
                CREATE TABLE provider_state (
                    source TEXT PRIMARY KEY NOT NULL,
                    day INTEGER NOT NULL,
                    daily_count INTEGER NOT NULL DEFAULT 0,
                    last_reserved INTEGER,
                    blocked_until INTEGER NOT NULL DEFAULT 0
                );
                PRAGMA user_version = 1;
                "#,
            )
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(Self { pool })
    }

    async fn trim(
        transaction: &mut Transaction<'_, Sqlite>,
        now: i64,
        max_entries: u32,
        keep: Option<&str>,
    ) -> Result<u64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM city_cache")
            .fetch_one(&mut **transaction)
            .await?;
        let excess = count.saturating_sub(i64::from(max_entries)).max(0);
        if excess == 0 {
            return Ok(0);
        }
        Ok(sqlx::query(
            r#"
            DELETE FROM city_cache WHERE ip IN (
                SELECT ip FROM city_cache
                WHERE (lease_until IS NULL OR lease_until <= ?)
                    AND (? IS NULL OR ip != ?)
                ORDER BY last_used, ip LIMIT ?
            )
            "#,
        )
        .bind(now)
        .bind(keep)
        .bind(keep)
        .bind(excess)
        .execute(&mut **transaction)
        .await?
        .rows_affected())
    }

    pub(super) async fn observe(
        &self,
        ip: IpAddr,
        now: i64,
        max_entries: u32,
    ) -> Result<Option<CachedCity>> {
        let key = ip_key(ip);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing =
            sqlx::query("SELECT location_json, source, updated_at FROM city_cache WHERE ip = ?")
                .bind(&key)
                .fetch_optional(&mut *transaction)
                .await?;
        if existing.is_some() {
            sqlx::query("UPDATE city_cache SET last_used = ? WHERE ip = ?")
                .bind(now)
                .bind(&key)
                .execute(&mut *transaction)
                .await?;
            // A reduced capacity must still preserve in-flight work.
            Self::trim(&mut transaction, now, max_entries, None).await?;
            let result = cached_city(existing)?;
            transaction.commit().await?;
            return Ok(result);
        }
        if max_entries == 0 {
            Self::trim(&mut transaction, now, 0, None).await?;
            transaction.commit().await?;
            return Ok(None);
        }
        Self::trim(&mut transaction, now, max_entries - 1, None).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM city_cache")
            .fetch_one(&mut *transaction)
            .await?;
        if count < i64::from(max_entries) {
            sqlx::query("INSERT INTO city_cache (ip, last_used) VALUES (?, ?)")
                .bind(key)
                .bind(now)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(None)
    }

    pub(super) async fn read(&self, ip: IpAddr) -> Result<Option<CachedCity>> {
        cached_city(
            sqlx::query("SELECT location_json, source, updated_at FROM city_cache WHERE ip = ?")
                .bind(ip_key(ip))
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub(super) async fn due(&self, now: i64, active_since: i64, limit: u32) -> Result<Vec<IpAddr>> {
        let keys: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT ip FROM city_cache
            WHERE last_used >= ? AND refresh_at <= ?
                AND (lease_until IS NULL OR lease_until <= ?)
            ORDER BY (location_json IS NOT NULL), refresh_at, last_used DESC, ip
            LIMIT ?
            "#,
        )
        .bind(active_since)
        .bind(now)
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        keys.into_iter()
            .map(|key| key.parse().context("invalid IP in city cache"))
            .collect()
    }

    pub(super) async fn claim(&self, ip: IpAddr, now: i64, active_since: i64) -> Result<bool> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let claimed = sqlx::query(
            r#"
            UPDATE city_cache SET lease_until = ?
            WHERE ip = ? AND last_used >= ? AND refresh_at <= ?
                AND (lease_until IS NULL OR lease_until <= ?)
            "#,
        )
        .bind(now.saturating_add(LEASE_SECONDS))
        .bind(ip_key(ip))
        .bind(active_since)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        transaction.commit().await?;
        Ok(claimed)
    }

    pub(super) async fn save_success(
        &self,
        ip: IpAddr,
        location: &CityLocation,
        source: &str,
        now: i64,
        next_refresh: i64,
    ) -> Result<()> {
        let location = serde_json::to_string(location)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            r#"
            UPDATE city_cache SET location_json = ?, source = ?, updated_at = ?,
                last_attempt = ?, refresh_at = ?, fail_count = 0, lease_until = NULL
            WHERE ip = ?
            "#,
        )
        .bind(location)
        .bind(source)
        .bind(now)
        .bind(now)
        .bind(next_refresh)
        .bind(ip_key(ip))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(super) async fn save_failure(&self, ip: IpAddr, now: i64, next_refresh: i64) -> Result<()> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            r#"
            UPDATE city_cache SET last_attempt = ?, refresh_at = ?,
                fail_count = MIN(fail_count + 1, 4294967295), lease_until = NULL
            WHERE ip = ?
            "#,
        )
        .bind(now)
        .bind(next_refresh)
        .bind(ip_key(ip))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(super) async fn defer(&self, ip: IpAddr, next_refresh: i64) -> Result<()> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE city_cache SET refresh_at = ?, lease_until = NULL WHERE ip = ?")
            .bind(next_refresh)
            .bind(ip_key(ip))
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(super) async fn failure_count(&self, ip: IpAddr) -> Result<u32> {
        let count: Option<i64> =
            sqlx::query_scalar("SELECT fail_count FROM city_cache WHERE ip = ?")
                .bind(ip_key(ip))
                .fetch_optional(&self.pool)
                .await?;
        Ok(count.unwrap_or(0).clamp(0, i64::from(u32::MAX)) as u32)
    }

    pub(super) async fn cleanup(
        &self,
        now: i64,
        retention_seconds: i64,
        max_entries: u32,
    ) -> Result<u64> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut removed = sqlx::query(
            r#"
            DELETE FROM city_cache WHERE last_used < ?
                AND (lease_until IS NULL OR lease_until <= ?)
            "#,
        )
        .bind(now.saturating_sub(retention_seconds.max(0)))
        .bind(now)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        removed += Self::trim(&mut transaction, now, max_entries, None).await?;
        transaction.commit().await?;
        if removed > 0 {
            let mut connection = self.pool.acquire().await?;
            sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                .execute(&mut *connection)
                .await?;
            sqlx::query("PRAGMA incremental_vacuum(128)")
                .execute(&mut *connection)
                .await?;
        }
        Ok(removed)
    }

    pub(super) async fn reserve_provider(
        &self,
        source: &str,
        now: i64,
        daily_limit: u32,
    ) -> Result<ProviderPermit> {
        let day = now.div_euclid(DAY_SECONDS);
        let next_day = day.saturating_add(1).saturating_mul(DAY_SECONDS);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO provider_state (source, day) VALUES (?, ?) ON CONFLICT(source) DO NOTHING",
        )
        .bind(source)
        .bind(day)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE provider_state SET day = ?, daily_count = 0 WHERE source = ? AND day != ?",
        )
        .bind(day)
        .bind(source)
        .bind(day)
        .execute(&mut *transaction)
        .await?;
        let row = sqlx::query(
            "SELECT daily_count, last_reserved, blocked_until FROM provider_state WHERE source = ?",
        )
        .bind(source)
        .fetch_one(&mut *transaction)
        .await?;
        let daily_count: i64 = row.try_get("daily_count")?;
        let last_reserved: Option<i64> = row.try_get("last_reserved")?;
        let blocked_until: i64 = row.try_get("blocked_until")?;
        let mut wait_until = now.max(blocked_until);
        if let Some(last_reserved) = last_reserved {
            wait_until = wait_until.max(last_reserved.saturating_add(1));
        }
        if daily_count >= i64::from(daily_limit) {
            wait_until = wait_until.max(next_day);
        }
        let permit = if wait_until > now {
            ProviderPermit::WaitUntil(wait_until)
        } else {
            sqlx::query(
                "UPDATE provider_state SET daily_count = daily_count + 1, last_reserved = ? WHERE source = ?",
            )
            .bind(now)
            .bind(source)
            .execute(&mut *transaction)
            .await?;
            ProviderPermit::Granted
        };
        transaction.commit().await?;
        Ok(permit)
    }

    pub(super) async fn block_provider(&self, source: &str, until: i64) -> Result<()> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            r#"
            INSERT INTO provider_state (source, day, blocked_until) VALUES (?, ?, ?)
            ON CONFLICT(source) DO UPDATE
                SET blocked_until = MAX(provider_state.blocked_until, excluded.blocked_until)
            "#,
        )
        .bind(source)
        .bind(until.div_euclid(DAY_SECONDS))
        .bind(until)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn remove_test_file(path: &Path) {
        for attempt in 0..20 {
            match tokio::fs::remove_file(path).await {
                Ok(()) => return,
                Err(error)
                    if cfg!(windows)
                        && matches!(error.raw_os_error(), Some(32 | 33))
                        && attempt < 19 =>
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("failed to remove test database: {error}"),
            }
        }
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn location() -> CityLocation {
        CityLocation {
            country: "NZ".to_string(),
            city: "Auckland".to_string(),
            region: Some("Auckland".to_string()),
            latitude: -36.85,
            longitude: 174.76,
            accuracy_radius_km: Some(25.0),
        }
    }

    async fn count(store: &Store) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM city_cache")
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    async fn last_used(store: &Store, address: IpAddr) -> i64 {
        sqlx::query_scalar("SELECT last_used FROM city_cache WHERE ip = ?")
            .bind(ip_key(address))
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn repeated_observation_and_ipv6_spelling_share_one_row() {
        let store = Store::memory().await.unwrap();
        let first = ip("2001:0db8:0000:0000:0000:0000:0000:0001");
        let canonical = ip("2001:db8::1");
        assert!(store.observe(first, 10, 10).await.unwrap().is_none());
        store
            .save_success(first, &location(), "test", 20, 100)
            .await
            .unwrap();
        let cached = store.observe(canonical, 30, 10).await.unwrap().unwrap();
        assert_eq!(cached.location, location());
        assert_eq!(count(&store).await, 1);
        assert_eq!(last_used(&store, first).await, 30);
        store.observe(ip("::ffff:192.0.2.1"), 40, 10).await.unwrap();
        store.observe(ip("192.0.2.1"), 50, 10).await.unwrap();
        assert_eq!(count(&store).await, 2);
    }

    #[tokio::test]
    async fn claims_are_exclusive_and_require_active_due_rows() {
        let store = Store::memory().await.unwrap();
        let address = ip("192.0.2.1");
        store.observe(address, 100, 10).await.unwrap();
        let (first, second) =
            tokio::join!(store.claim(address, 100, 90), store.claim(address, 100, 90));
        assert_ne!(first.unwrap(), second.unwrap());
        assert!(store.due(219, 90, 10).await.unwrap().is_empty());
        assert_eq!(store.due(220, 90, 10).await.unwrap(), vec![address]);
        assert!(!store.claim(address, 220, 101).await.unwrap());
        assert!(store.claim(address, 220, 90).await.unwrap());
        store.defer(address, 500).await.unwrap();
        assert_eq!(store.failure_count(address).await.unwrap(), 0);
        assert!(!store.claim(address, 499, 90).await.unwrap());
        assert!(store.claim(address, 500, 90).await.unwrap());
    }

    #[tokio::test]
    async fn refresh_results_do_not_make_old_rows_active() {
        let store = Store::memory().await.unwrap();
        let address = ip("192.0.2.1");
        store.observe(address, 100, 10).await.unwrap();
        store.save_failure(address, 150, 160).await.unwrap();
        assert_eq!(store.failure_count(address).await.unwrap(), 1);
        assert_eq!(last_used(&store, address).await, 100);
        store
            .save_success(address, &location(), "test", 200, 210)
            .await
            .unwrap();
        assert_eq!(store.failure_count(address).await.unwrap(), 0);
        assert_eq!(last_used(&store, address).await, 100);
        store.save_failure(address, 220, 230).await.unwrap();
        let cached = store.read(address).await.unwrap().unwrap();
        assert_eq!(cached.updated_at, 200);
        assert_eq!(cached.location, location());
        assert_eq!(last_used(&store, address).await, 100);
        assert!(store.due(250, 101, 10).await.unwrap().is_empty());
        assert_eq!(store.cleanup(250, 100, 10).await.unwrap(), 1);
        assert!(store.read(address).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unlocated_rows_are_due_before_refreshes() {
        let store = Store::memory().await.unwrap();
        let cached = ip("192.0.2.1");
        let missing = ip("192.0.2.2");
        store.observe(cached, 100, 10).await.unwrap();
        store.observe(missing, 101, 10).await.unwrap();
        store
            .save_success(cached, &location(), "test", 105, 110)
            .await
            .unwrap();
        assert_eq!(store.due(120, 90, 1).await.unwrap(), vec![missing]);
        assert_eq!(store.due(120, 90, 10).await.unwrap(), vec![missing, cached]);
    }

    #[tokio::test]
    async fn lru_capacity_and_retention_never_remove_active_leases() {
        let store = Store::memory().await.unwrap();
        let first = ip("192.0.2.1");
        let second = ip("192.0.2.2");
        let third = ip("192.0.2.3");
        store.observe(first, 10, 2).await.unwrap();
        store.observe(second, 20, 2).await.unwrap();
        store.observe(first, 30, 2).await.unwrap();
        store.observe(third, 40, 2).await.unwrap();
        assert_eq!(count(&store).await, 2);
        assert_eq!(store.due(40, 0, 10).await.unwrap(), vec![third, first]);
        assert!(store.claim(first, 40, 0).await.unwrap());
        assert!(store.claim(third, 40, 0).await.unwrap());
        store.observe(second, 50, 2).await.unwrap();
        assert_eq!(count(&store).await, 2);
        assert_eq!(store.cleanup(100, 1, 0).await.unwrap(), 0);
        assert_eq!(count(&store).await, 2);
        assert_eq!(store.cleanup(160, 1, 0).await.unwrap(), 2);
        store.observe(first, 170, 0).await.unwrap();
        assert_eq!(count(&store).await, 0);
    }

    #[tokio::test]
    async fn cleanup_applies_lru_cap_without_refreshing_usage() {
        let store = Store::memory().await.unwrap();
        for (value, now) in [("192.0.2.1", 10), ("192.0.2.2", 20), ("192.0.2.3", 30)] {
            store.observe(ip(value), now, 10).await.unwrap();
        }
        assert_eq!(store.cleanup(40, 100, 1).await.unwrap(), 2);
        assert_eq!(store.due(40, 0, 10).await.unwrap(), vec![ip("192.0.2.3")]);
    }

    #[tokio::test]
    async fn provider_budget_interval_day_reset_and_cooldown() {
        let store = Store::memory().await.unwrap();
        assert_eq!(
            store.reserve_provider("test", 10, 2).await.unwrap(),
            ProviderPermit::Granted
        );
        assert_eq!(
            store.reserve_provider("test", 10, 2).await.unwrap(),
            ProviderPermit::WaitUntil(11)
        );
        assert_eq!(
            store.reserve_provider("test", 11, 2).await.unwrap(),
            ProviderPermit::Granted
        );
        assert_eq!(
            store.reserve_provider("test", 12, 2).await.unwrap(),
            ProviderPermit::WaitUntil(DAY_SECONDS)
        );
        store
            .block_provider("test", DAY_SECONDS + 50)
            .await
            .unwrap();
        store
            .block_provider("test", DAY_SECONDS + 10)
            .await
            .unwrap();
        assert_eq!(
            store
                .reserve_provider("test", DAY_SECONDS, 2)
                .await
                .unwrap(),
            ProviderPermit::WaitUntil(DAY_SECONDS + 50)
        );
        assert_eq!(
            store
                .reserve_provider("test", DAY_SECONDS + 50, 2)
                .await
                .unwrap(),
            ProviderPermit::Granted
        );
        assert_eq!(
            store.reserve_provider("other", 10, 0).await.unwrap(),
            ProviderPermit::WaitUntil(DAY_SECONDS)
        );
    }

    #[tokio::test]
    async fn parallel_provider_reservations_grant_only_one() {
        let store = Store::memory().await.unwrap();
        let (first, second) = tokio::join!(
            store.reserve_provider("test", 100, 10),
            store.reserve_provider("test", 100, 10)
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first, second);
        assert!(matches!(
            first,
            ProviderPermit::Granted | ProviderPermit::WaitUntil(101)
        ));
        assert!(matches!(
            second,
            ProviderPermit::Granted | ProviderPermit::WaitUntil(101)
        ));
    }

    #[tokio::test]
    async fn cached_results_budget_and_cooldown_survive_reopening() {
        let path = std::env::temp_dir().join(format!("easytier-city-{}.db", uuid::Uuid::new_v4()));
        let store = Store::open(&path).await.unwrap();
        let address = ip("192.0.2.1");
        store.observe(address, 100, 10).await.unwrap();
        store
            .save_success(address, &location(), "test", 110, 200)
            .await
            .unwrap();
        assert_eq!(
            store.reserve_provider("test", 110, 1).await.unwrap(),
            ProviderPermit::Granted
        );
        store
            .block_provider("test", DAY_SECONDS + 10)
            .await
            .unwrap();
        store.pool.close().await;
        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(
            reopened.read(address).await.unwrap().unwrap(),
            CachedCity {
                location: location(),
                source: "test".to_string(),
                updated_at: 110,
            }
        );
        assert_eq!(last_used(&reopened, address).await, 100);
        assert_eq!(
            reopened.reserve_provider("test", 111, 1).await.unwrap(),
            ProviderPermit::WaitUntil(DAY_SECONDS + 10)
        );
        assert_eq!(
            reopened
                .reserve_provider("test", DAY_SECONDS, 1)
                .await
                .unwrap(),
            ProviderPermit::WaitUntil(DAY_SECONDS + 10)
        );
        reopened.pool.close().await;
        drop(reopened);
        drop(store);
        remove_test_file(&path).await;
    }

    #[tokio::test]
    async fn unknown_schema_versions_are_rejected() {
        let path = std::env::temp_dir().join(format!("easytier-city-{}.db", uuid::Uuid::new_v4()));
        let store = Store::open(&path).await.unwrap();
        sqlx::query("PRAGMA user_version = 2")
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;
        assert!(Store::open(&path).await.is_err());
        drop(store);
        remove_test_file(&path).await;
    }

    #[tokio::test]
    async fn unrelated_database_schema_is_not_modified() {
        let path = std::env::temp_dir().join(format!("easytier-other-{}.db", uuid::Uuid::new_v4()));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE users (name TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(Store::open(&path).await.is_err());
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(tables, vec!["users".to_string()]);
        pool.close().await;
        drop(pool);
        remove_test_file(&path).await;
    }
}
