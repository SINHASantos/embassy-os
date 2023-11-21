use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use helpers::to_tmp_path;
use josekit::jwk::Jwk;
use patch_db::json_ptr::JsonPointer;
use patch_db::PatchDb;
use reqwest::{Client, Proxy, Url};
use rpc_toolkit::Context;
use serde::Deserialize;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use tokio::sync::{broadcast, oneshot, Mutex, RwLock};
use tokio::time::Instant;
use tracing::instrument;

use super::setup::CURRENT_SECRET;
use crate::account::AccountInfo;
use crate::core::rpc_continuations::{RequestGuid, RestHandler, RpcContinuation};
use crate::db::model::{CurrentDependents, Database, PackageDataEntryMatchModelRef};
use crate::db::prelude::PatchDbExt;
use crate::dependencies::compute_dependency_config_errs;
use crate::disk::OsPartitionInfo;
use crate::init::{check_time_is_synchronized, init_postgres};
use crate::install::cleanup::{cleanup_failed, uninstall};
use crate::manager::ManagerMap;
use crate::middleware::auth::HashSessionToken;
use crate::net::net_controller::NetController;
use crate::net::ssl::{root_ca_start_time, SslManager};
use crate::net::wifi::WpaCli;
use crate::notifications::NotificationManager;
use crate::shutdown::Shutdown;
use crate::status::MainStatus;
use crate::system::get_mem_info;
use crate::util::config::load_config_from_paths;
use crate::util::lshw::{lshw, LshwDevice};
use crate::{Error, ErrorKind, ResultExt};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RpcContextConfig {
    pub wifi_interface: Option<String>,
    pub ethernet_interface: String,
    pub os_partitions: OsPartitionInfo,
    pub migration_batch_rows: Option<usize>,
    pub migration_prefetch_rows: Option<usize>,
    pub bind_rpc: Option<SocketAddr>,
    pub tor_control: Option<SocketAddr>,
    pub tor_socks: Option<SocketAddr>,
    pub dns_bind: Option<Vec<SocketAddr>>,
    pub revision_cache_size: Option<usize>,
    pub datadir: Option<PathBuf>,
    pub log_server: Option<Url>,
}
impl RpcContextConfig {
    pub async fn load<P: AsRef<Path> + Send + 'static>(path: Option<P>) -> Result<Self, Error> {
        tokio::task::spawn_blocking(move || {
            load_config_from_paths(
                path.as_ref()
                    .into_iter()
                    .map(|p| p.as_ref())
                    .chain(std::iter::once(Path::new(
                        crate::util::config::DEVICE_CONFIG_PATH,
                    )))
                    .chain(std::iter::once(Path::new(crate::util::config::CONFIG_PATH))),
            )
        })
        .await
        .unwrap()
    }
    pub fn datadir(&self) -> &Path {
        self.datadir
            .as_deref()
            .unwrap_or_else(|| Path::new("/embassy-data"))
    }
    pub async fn db(&self, account: &AccountInfo) -> Result<PatchDb, Error> {
        let db_path = self.datadir().join("main").join("embassy.db");
        let db = PatchDb::open(&db_path)
            .await
            .with_ctx(|_| (crate::ErrorKind::Filesystem, db_path.display().to_string()))?;
        if !db.exists(&<JsonPointer>::default()).await {
            db.put(&<JsonPointer>::default(), &Database::init(account))
                .await?;
        }
        Ok(db)
    }
    #[instrument(skip_all)]
    pub async fn secret_store(&self) -> Result<PgPool, Error> {
        init_postgres(self.datadir()).await?;
        let secret_store =
            PgPool::connect_with(PgConnectOptions::new().database("secrets").username("root"))
                .await?;
        sqlx::migrate!()
            .run(&secret_store)
            .await
            .with_kind(crate::ErrorKind::Database)?;
        Ok(secret_store)
    }
}

pub struct RpcContextSeed {
    is_closed: AtomicBool,
    pub os_partitions: OsPartitionInfo,
    pub wifi_interface: Option<String>,
    pub ethernet_interface: String,
    pub datadir: PathBuf,
    pub disk_guid: Arc<String>,
    pub db: PatchDb,
    pub secret_store: PgPool,
    pub account: RwLock<AccountInfo>,
    pub net_controller: Arc<NetController>,
    pub managers: ManagerMap,
    pub metrics_cache: RwLock<Option<crate::system::Metrics>>,
    pub shutdown: broadcast::Sender<Option<Shutdown>>,
    pub tor_socks: SocketAddr,
    pub notification_manager: NotificationManager,
    pub open_authed_websockets: Mutex<BTreeMap<HashSessionToken, Vec<oneshot::Sender<()>>>>,
    pub rpc_stream_continuations: Mutex<BTreeMap<RequestGuid, RpcContinuation>>,
    pub wifi_manager: Option<Arc<RwLock<WpaCli>>>,
    pub current_secret: Arc<Jwk>,
    pub client: Client,
    pub hardware: Hardware,
    pub start_time: Instant,
}

pub struct Hardware {
    pub devices: Vec<LshwDevice>,
    pub ram: u64,
}

#[derive(Clone)]
pub struct RpcContext(Arc<RpcContextSeed>);
impl RpcContext {
    #[instrument(skip_all)]
    pub async fn init<P: AsRef<Path> + Send + Sync + 'static>(
        cfg_path: Option<P>,
        disk_guid: Arc<String>,
    ) -> Result<Self, Error> {
        let base = RpcContextConfig::load(cfg_path).await?;
        tracing::info!("Loaded Config");
        let tor_proxy = base.tor_socks.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(127, 0, 0, 1),
            9050,
        )));
        let (shutdown, _) = tokio::sync::broadcast::channel(1);
        let secret_store = base.secret_store().await?;
        tracing::info!("Opened Pg DB");
        let account = AccountInfo::load(&secret_store).await?;
        let db = base.db(&account).await?;
        tracing::info!("Opened PatchDB");
        let net_controller = Arc::new(
            NetController::init(
                base.tor_control
                    .unwrap_or(SocketAddr::from(([127, 0, 0, 1], 9051))),
                tor_proxy,
                base.dns_bind
                    .as_deref()
                    .unwrap_or(&[SocketAddr::from(([127, 0, 0, 1], 53))]),
                SslManager::new(&account, root_ca_start_time().await?)?,
                &account.hostname,
                &account.key,
            )
            .await?,
        );
        tracing::info!("Initialized Net Controller");
        let managers = ManagerMap::default();
        let metrics_cache = RwLock::<Option<crate::system::Metrics>>::new(None);
        let notification_manager = NotificationManager::new(secret_store.clone());
        tracing::info!("Initialized Notification Manager");
        let tor_proxy_url = format!("socks5h://{tor_proxy}");
        let devices = lshw().await?;
        let ram = get_mem_info().await?.total.0 as u64 * 1024 * 1024;

        if !db.peek().await.as_server_info().as_ntp_synced().de()? {
            let db = db.clone();
            tokio::spawn(async move {
                while !check_time_is_synchronized().await.unwrap() {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                db.mutate(|v| v.as_server_info_mut().as_ntp_synced_mut().ser(&true))
                    .await
                    .unwrap()
            });
        }

        let seed = Arc::new(RpcContextSeed {
            is_closed: AtomicBool::new(false),
            datadir: base.datadir().to_path_buf(),
            os_partitions: base.os_partitions,
            wifi_interface: base.wifi_interface.clone(),
            ethernet_interface: base.ethernet_interface,
            disk_guid,
            db,
            secret_store,
            account: RwLock::new(account),
            net_controller,
            managers,
            metrics_cache,
            shutdown,
            tor_socks: tor_proxy,
            notification_manager,
            open_authed_websockets: Mutex::new(BTreeMap::new()),
            rpc_stream_continuations: Mutex::new(BTreeMap::new()),
            wifi_manager: base
                .wifi_interface
                .map(|i| Arc::new(RwLock::new(WpaCli::init(i)))),
            current_secret: Arc::new(
                Jwk::generate_ec_key(josekit::jwk::alg::ec::EcCurve::P256).map_err(|e| {
                    tracing::debug!("{:?}", e);
                    tracing::error!("Couldn't generate ec key");
                    Error::new(
                        color_eyre::eyre::eyre!("Couldn't generate ec key"),
                        crate::ErrorKind::Unknown,
                    )
                })?,
            ),
            client: Client::builder()
                .proxy(Proxy::custom(move |url| {
                    if url.host_str().map_or(false, |h| h.ends_with(".onion")) {
                        Some(tor_proxy_url.clone())
                    } else {
                        None
                    }
                }))
                .build()
                .with_kind(crate::ErrorKind::ParseUrl)?,
            hardware: Hardware { devices, ram },
            start_time: Instant::now(),
        });

        let res = Self(seed.clone());
        res.cleanup_and_initialize().await?;
        tracing::info!("Cleaned up transient states");
        Ok(res)
    }

    #[instrument(skip_all)]
    pub async fn shutdown(self) -> Result<(), Error> {
        self.managers.empty().await?;
        self.secret_store.close().await;
        self.is_closed.store(true, Ordering::SeqCst);
        tracing::info!("RPC Context is shutdown");
        // TODO: shutdown http servers
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn cleanup_and_initialize(&self) -> Result<(), Error> {
        self.db
            .mutate(|f| {
                let mut current_dependents = f
                    .as_package_data()
                    .keys()?
                    .into_iter()
                    .map(|k| (k.clone(), BTreeMap::new()))
                    .collect::<BTreeMap<_, _>>();
                for (package_id, package) in f.as_package_data_mut().as_entries_mut()? {
                    for (k, v) in package
                        .as_installed_mut()
                        .into_iter()
                        .flat_map(|i| i.clone().into_current_dependencies().into_entries())
                        .flatten()
                    {
                        let mut entry: BTreeMap<_, _> =
                            current_dependents.remove(&k).unwrap_or_default();
                        entry.insert(package_id.clone(), v.de()?);
                        current_dependents.insert(k, entry);
                    }
                }
                for (package_id, current_dependents) in current_dependents {
                    if let Some(deps) = f
                        .as_package_data_mut()
                        .as_idx_mut(&package_id)
                        .and_then(|pde| pde.expect_as_installed_mut().ok())
                        .map(|i| i.as_installed_mut().as_current_dependents_mut())
                    {
                        deps.ser(&CurrentDependents(current_dependents))?;
                    } else if let Some(deps) = f
                        .as_package_data_mut()
                        .as_idx_mut(&package_id)
                        .and_then(|pde| pde.expect_as_removing_mut().ok())
                        .map(|i| i.as_removing_mut().as_current_dependents_mut())
                    {
                        deps.ser(&CurrentDependents(current_dependents))?;
                    }
                }
                Ok(())
            })
            .await?;

        let peek = self.db.peek().await;

        for (package_id, package) in peek.as_package_data().as_entries()?.into_iter() {
            let action = match package.as_match() {
                PackageDataEntryMatchModelRef::Installing(_)
                | PackageDataEntryMatchModelRef::Restoring(_)
                | PackageDataEntryMatchModelRef::Updating(_) => {
                    cleanup_failed(self, &package_id).await
                }
                PackageDataEntryMatchModelRef::Removing(_) => {
                    uninstall(
                        self,
                        self.secret_store.acquire().await?.as_mut(),
                        &package_id,
                    )
                    .await
                }
                PackageDataEntryMatchModelRef::Installed(m) => {
                    let version = m.as_manifest().as_version().clone().de()?;
                    let volumes = m.as_manifest().as_volumes().de()?;
                    for (volume_id, volume_info) in &*volumes {
                        let tmp_path = to_tmp_path(volume_info.path_for(
                            &self.datadir,
                            &package_id,
                            &version,
                            volume_id,
                        ))
                        .with_kind(ErrorKind::Filesystem)?;
                        if tokio::fs::metadata(&tmp_path).await.is_ok() {
                            tokio::fs::remove_dir_all(&tmp_path).await?;
                        }
                    }
                    Ok(())
                }
                _ => continue,
            };
            if let Err(e) = action {
                tracing::error!("Failed to clean up package {}: {}", package_id, e);
                tracing::debug!("{:?}", e);
            }
        }
        let peek = self
            .db
            .mutate(|v| {
                for (_, pde) in v.as_package_data_mut().as_entries_mut()? {
                    let status = pde
                        .expect_as_installed_mut()?
                        .as_installed_mut()
                        .as_status_mut()
                        .as_main_mut();
                    let running = status.clone().de()?.running();
                    status.ser(&if running {
                        MainStatus::Starting
                    } else {
                        MainStatus::Stopped
                    })?;
                }
                Ok(v.clone())
            })
            .await?;
        self.managers.init(self.clone(), peek.clone()).await?;
        tracing::info!("Initialized Package Managers");

        let mut all_dependency_config_errs = BTreeMap::new();
        for (package_id, package) in peek.as_package_data().as_entries()?.into_iter() {
            let package = package.clone();
            if let Some(current_dependencies) = package
                .as_installed()
                .and_then(|x| x.as_current_dependencies().de().ok())
            {
                let manifest = package.as_manifest().de()?;
                all_dependency_config_errs.insert(
                    package_id.clone(),
                    compute_dependency_config_errs(
                        self,
                        &peek,
                        &manifest,
                        &current_dependencies,
                        &Default::default(),
                    )
                    .await?,
                );
            }
        }
        self.db
            .mutate(|v| {
                for (package_id, errs) in all_dependency_config_errs {
                    if let Some(config_errors) = v
                        .as_package_data_mut()
                        .as_idx_mut(&package_id)
                        .and_then(|pde| pde.as_installed_mut())
                        .map(|i| i.as_status_mut().as_dependency_config_errors_mut())
                    {
                        config_errors.ser(&errs)?;
                    }
                }
                Ok(())
            })
            .await?;

        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn clean_continuations(&self) {
        let mut continuations = self.rpc_stream_continuations.lock().await;
        let mut to_remove = Vec::new();
        for (guid, cont) in &*continuations {
            if cont.is_timed_out() {
                to_remove.push(guid.clone());
            }
        }
        for guid in to_remove {
            continuations.remove(&guid);
        }
    }

    #[instrument(skip_all)]
    pub async fn add_continuation(&self, guid: RequestGuid, handler: RpcContinuation) {
        self.clean_continuations().await;
        self.rpc_stream_continuations
            .lock()
            .await
            .insert(guid, handler);
    }

    pub async fn get_continuation_handler(&self, guid: &RequestGuid) -> Option<RestHandler> {
        let mut continuations = self.rpc_stream_continuations.lock().await;
        if let Some(cont) = continuations.remove(guid) {
            cont.into_handler().await
        } else {
            None
        }
    }

    pub async fn get_ws_continuation_handler(&self, guid: &RequestGuid) -> Option<RestHandler> {
        let continuations = self.rpc_stream_continuations.lock().await;
        if matches!(continuations.get(guid), Some(RpcContinuation::WebSocket(_))) {
            drop(continuations);
            self.get_continuation_handler(guid).await
        } else {
            None
        }
    }

    pub async fn get_rest_continuation_handler(&self, guid: &RequestGuid) -> Option<RestHandler> {
        let continuations = self.rpc_stream_continuations.lock().await;
        if matches!(continuations.get(guid), Some(RpcContinuation::Rest(_))) {
            drop(continuations);
            self.get_continuation_handler(guid).await
        } else {
            None
        }
    }
}
impl AsRef<Jwk> for RpcContext {
    fn as_ref(&self) -> &Jwk {
        &CURRENT_SECRET
    }
}
impl Context for RpcContext {}
impl Deref for RpcContext {
    type Target = RpcContextSeed;
    fn deref(&self) -> &Self::Target {
        #[cfg(feature = "unstable")]
        if self.0.is_closed.load(Ordering::SeqCst) {
            panic!(
                "RpcContext used after shutdown! {}",
                tracing_error::SpanTrace::capture()
            );
        }
        &self.0
    }
}
impl Drop for RpcContext {
    fn drop(&mut self) {
        #[cfg(feature = "unstable")]
        if self.0.is_closed.load(Ordering::SeqCst) {
            tracing::info!(
                "RpcContext dropped. {} left.",
                Arc::strong_count(&self.0) - 1
            );
        }
    }
}
