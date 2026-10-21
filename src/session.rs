use crate::snapshot::{ParsedSnapshot, SnapshotCache};
use base64::Engine as _;
use dashmap::DashMap;
use dfs_daemon::store::s3::S3Store;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// How long a parsed snapshot is reused before it is fetched from S3 again.
const SNAPSHOT_CACHE_SECS: u64 = 30;

pub struct Session {
    pub s3: Arc<S3Store>,
    pub master_key: Option<[u8; 32]>,
    pub created_at: Instant,
    pub cache: RwLock<Option<SnapshotCache>>,
    /// Stable device ID used when writing browser-uploaded files into S3 snapshots.
    pub browser_device_id: uuid::Uuid,
}

impl Session {
    pub async fn get_snapshot(&self) -> anyhow::Result<Arc<ParsedSnapshot>> {
        {
            let guard = self.cache.read().await;
            if let Some(ref c) = *guard {
                if c.age_secs() < SNAPSHOT_CACHE_SECS {
                    return Ok(c.snapshot.clone());
                }
            }
        }
        let snap = Arc::new(ParsedSnapshot::load_from_s3(&self.s3).await?);
        *self.cache.write().await = Some(SnapshotCache {
            snapshot: snap.clone(),
            loaded_at: Instant::now(),
        });
        Ok(snap)
    }
}

pub struct SessionStore {
    inner: DashMap<String, Arc<Session>>,
    ttl: Duration,
}

impl SessionStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: DashMap::new(),
            ttl,
        }
    }

    pub fn create(&self, s3: Arc<S3Store>, master_key: Option<[u8; 32]>) -> String {
        let token = generate_token();
        self.inner.insert(
            token.clone(),
            Arc::new(Session {
                s3,
                master_key,
                created_at: Instant::now(),
                cache: RwLock::new(None),
                browser_device_id: uuid::Uuid::now_v7(),
            }),
        );
        token
    }

    pub fn get(&self, token: &str) -> Option<Arc<Session>> {
        let sess = self.inner.get(token).map(|s| s.clone())?;
        if sess.created_at.elapsed() > self.ttl {
            self.inner.remove(token);
            return None;
        }
        Some(sess)
    }

    pub fn remove(&self, token: &str) {
        self.inner.remove(token);
    }

    /// Periodically drop expired sessions (and the keys they hold) from memory.
    pub fn spawn_reaper(self: Arc<Self>) {
        actix_web::rt::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            loop {
                tick.tick().await;
                let ttl = self.ttl;
                self.inner.retain(|_, s| s.created_at.elapsed() <= ttl);
            }
        });
    }
}

fn generate_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
