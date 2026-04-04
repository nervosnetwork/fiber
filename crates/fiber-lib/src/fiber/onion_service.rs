use base64::Engine;
use std::borrow::Cow;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use tentacle::multiaddr::Multiaddr;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use torut::control::{
    AsyncEvent, AuthenticatedConn, ConnError, TorAuthData, TorAuthMethod, UnauthenticatedConn,
    COOKIE_LENGTH,
};
use torut::onion::TorSecretKeyV3;
use tracing::{debug, error, info, warn};

use futures::future::BoxFuture;

/// Configuration for the onion service, constructed from FiberConfig fields.
pub struct OnionServiceConfig {
    /// Path to store onion private key
    pub onion_private_key_path: String,
    /// Tor controller url, e.g. "127.0.0.1:9051"
    pub tor_controller: String,
    /// Tor controller password
    pub tor_password: Option<String>,
    /// The local p2p listen address that the onion service will forward traffic to
    pub p2p_listen_address: SocketAddr,
    /// The external port exposed by the onion service
    pub onion_external_port: u16,
}

/// Onion service that registers a Tor hidden service and monitors its health.
pub struct OnionService {
    key: TorSecretKeyV3,
    config: OnionServiceConfig,
}

impl OnionService {
    /// Create a new onion service. Returns the service and its `.onion` multiaddr.
    pub fn new(
        config: OnionServiceConfig,
        peer_id: &str,
    ) -> Result<(OnionService, Multiaddr), String> {
        let key = load_or_create_tor_secret_key(&config.onion_private_key_path)?;

        let tor_address_without_dot_onion = key
            .public()
            .get_onion_address()
            .get_address_without_dot_onion();

        let onion_multi_addr_str = format!(
            "/onion3/{}:{}/p2p/{}",
            tor_address_without_dot_onion, config.onion_external_port, peer_id
        );
        let onion_multi_addr = Multiaddr::from_str(&onion_multi_addr_str).map_err(|err| {
            format!(
                "Failed to parse onion address {} to multiaddr: {:?}",
                onion_multi_addr_str, err
            )
        })?;

        Ok((OnionService { config, key }, onion_multi_addr))
    }

    /// Start the onion service. This will loop and reconnect if the connection to Tor Server drops.
    /// The provided `cancel_token` can be used to gracefully shut down.
    /// When Tor reconnects after a failure, a notification is sent via `reconnect_notify`
    /// so the caller can trigger peer reconnection.
    /// The `ready_tx` oneshot is used to signal the caller that the first successful
    /// registration with Tor has completed (or that it failed fatally).
    pub async fn start(
        &self,
        cancel_token: CancellationToken,
        reconnect_notify: tokio::sync::mpsc::UnboundedSender<()>,
        ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    ) -> Result<(), String> {
        let mut first_start = true;
        let mut ready_tx = Some(ready_tx);
        loop {
            let (tor_alive_tx, mut tor_alive_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            match self
                .launch_onion_service(cancel_token.clone(), tor_alive_tx)
                .await
            {
                Ok(_) => {
                    info!("Onion service started successfully");
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Ok(()));
                    }
                    if !first_start {
                        // Tor has reconnected after a failure, notify the network actor
                        // to re-establish peer connections that were dropped.
                        info!("Tor reconnected, notifying network to reconnect peers");
                        let _ = reconnect_notify.send(());
                    }
                }
                Err(err) => {
                    error!("Failed to start onion service: {}", err);
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(err));
                        return Ok(());
                    }
                }
            }
            first_start = false;
            // Wait until tor connection drops or cancellation
            tokio::select! {
                _ = tor_alive_rx.recv() => {}
                _ = cancel_token.cancelled() => {
                    info!("Onion service received stop signal, exiting...");
                    return Ok(());
                }
            }

            if cancel_token.is_cancelled() {
                return Ok(());
            }

            warn!(
                "Connection to tor controller ({}) appears lost, retrying...",
                self.config.tor_controller
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn launch_onion_service(
        &self,
        cancel_token: CancellationToken,
        tor_alive_tx: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<(), String> {
        let mut tor_controller = TorController::new(
            &self.config.tor_controller,
            self.config.tor_password.clone(),
        )
        .await?;

        tor_controller.wait_bootstrap_done(&cancel_token).await?;

        let listeners = [(
            self.config.onion_external_port,
            self.config.p2p_listen_address,
        )];
        info!(
            "Adding onion service v3: external port {} -> {}",
            self.config.onion_external_port, self.config.p2p_listen_address
        );
        tor_controller
            .add_onion_v3(self.key.clone(), &mut listeners.iter())
            .await
            .map_err(|err| {
                format!(
                    "Failed to add onion service (Port={},{}): {:?}",
                    self.config.onion_external_port, self.config.p2p_listen_address, err
                )
            })?;
        info!(
            "Added onion service v3, forwarding to {}",
            self.config.p2p_listen_address
        );

        // Monitor tor uptime in background
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(3));
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(err) = tor_controller.get_uptime().await {
                            error!("Failed to get tor server uptime: {:?}", err);
                            drop(tor_alive_tx);
                            return;
                        }
                    }
                    _ = cancel_token.cancelled() => {
                        info!("Onion service monitor received stop signal, exiting...");
                        drop(tor_alive_tx);
                        return;
                    }
                }
            }
        });

        Ok(())
    }
}

type TorAuthenticatedConn =
    AuthenticatedConn<TcpStream, fn(AsyncEvent<'_>) -> BoxFuture<'static, Result<(), ConnError>>>;

/// Controller for communicating with the Tor daemon via its control port.
struct TorController {
    inner: TorAuthenticatedConn,
}

impl TorController {
    async fn new(tor_controller_url: &str, tor_password: Option<String>) -> Result<Self, String> {
        let s = TcpStream::connect(tor_controller_url)
            .await
            .map_err(|err| {
                format!(
                    "Failed to connect to tor controller {}: {:?}",
                    tor_controller_url, err
                )
            })?;

        let mut utc: UnauthenticatedConn<TcpStream> = UnauthenticatedConn::new(s);

        authenticate(tor_password, &mut utc).await?;

        let ac = utc.into_authenticated().await;

        Ok(TorController { inner: ac })
    }

    async fn get_uptime(&mut self) -> Result<Duration, ConnError> {
        let uptime = self.inner.get_info("uptime").await.map_err(|err| {
            warn!(
                "Failed to get uptime; the Tor controller may not expose 'uptime' or returned an error: {}",
                err
            );
            err
        })?;
        debug!("tor server uptime: {} seconds", uptime);
        let secs: u64 = uptime.parse().map_err(|err| {
            ConnError::IOError(std::io::Error::other(format!(
                "failed to parse uptime {} to u64: {}",
                uptime, err
            )))
        })?;
        Ok(Duration::from_secs(secs))
    }

    async fn wait_bootstrap_done(
        &mut self,
        cancel_token: &CancellationToken,
    ) -> Result<(), String> {
        info!("Waiting for Tor server to bootstrap...");
        loop {
            if cancel_token.is_cancelled() {
                return Err("Received stop signal while waiting for Tor bootstrap".to_string());
            }
            match self.inner.get_info_unquote("status/bootstrap-phase").await {
                Ok(info) => {
                    info!("Tor bootstrap status: {:?}", info);
                    if info.contains("Done") {
                        break;
                    }
                }
                Err(err) => {
                    return Err(format!("Failed to get tor bootstrap status: {:?}", err));
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        info!("Tor server bootstrap done!");
        Ok(())
    }

    async fn add_onion_v3(
        &mut self,
        key: TorSecretKeyV3,
        listeners: &mut impl Iterator<Item = &(u16, SocketAddr)>,
    ) -> Result<(), ConnError> {
        self.inner
            .add_onion_v3(&key, false, false, false, None, listeners)
            .await
    }
}

async fn authenticate(
    tor_password: Option<String>,
    utc: &mut UnauthenticatedConn<TcpStream>,
) -> Result<(), String> {
    let proto_info = utc
        .load_protocol_info()
        .await
        .map_err(|err| format!("Failed to load protocol info: {:?}", err))?;

    proto_info.auth_methods.iter().for_each(|m| {
        info!("Tor controller supports auth method: {:?}", m);
    });

    if proto_info.auth_methods.contains(&TorAuthMethod::Null) {
        utc.authenticate(&TorAuthData::Null)
            .await
            .map_err(|err| format!("Failed to authenticate with null: {:?}", err))?;
        if tor_password.is_some() {
            warn!(
                "Password not required for the Tor controller, but `tor_password` is configured."
            );
        }
        return Ok(());
    }

    if proto_info
        .auth_methods
        .contains(&TorAuthMethod::HashedPassword)
    {
        if let Some(tor_password) = tor_password.clone() {
            utc.authenticate(&TorAuthData::HashedPassword(Cow::Owned(tor_password)))
                .await
                .map_err(|err| format!("Failed to authenticate with password: {:?}", err))?;
            return Ok(());
        } else {
            warn!("Tor server requires a password, but none is configured");
        }
    }

    if proto_info.auth_methods.contains(&TorAuthMethod::Cookie)
        || proto_info.auth_methods.contains(&TorAuthMethod::SafeCookie)
    {
        let cookie = load_auth_cookie(proto_info).await?;
        let tor_auth_data = if proto_info.auth_methods.contains(&TorAuthMethod::Cookie) {
            debug!("Using Cookie auth method...");
            TorAuthData::Cookie(Cow::Owned(cookie))
        } else {
            debug!("Using SafeCookie auth method...");
            TorAuthData::SafeCookie(Cow::Owned(cookie))
        };
        utc.authenticate(&tor_auth_data)
            .await
            .map_err(|err| format!("Failed to authenticate with cookie: {:?}", err))?;
        return Ok(());
    }

    Err(format!(
        "Tor server does not support any known authentication method; proto_info: {:?}",
        proto_info
    ))
}

async fn load_auth_cookie(
    proto_info: &torut::control::TorPreAuthInfo<'_>,
) -> Result<Vec<u8>, String> {
    let cookie_path = proto_info
        .cookie_file
        .as_ref()
        .ok_or_else(|| "Tor server did not provide cookie file path".to_string())?;
    let mut cookie_file = File::open(cookie_path.as_ref())
        .await
        .map_err(|err| format!("Failed to open cookie file: {:?}", err))?;
    let mut cookie = Vec::new();
    cookie_file
        .read_to_end(&mut cookie)
        .await
        .map_err(|err| format!("Failed to read cookie file: {:?}", err))?;
    if cookie.len() != COOKIE_LENGTH {
        return Err(format!(
            "Invalid cookie length: expected {}, got {}",
            COOKIE_LENGTH,
            cookie.len()
        ));
    }
    Ok(cookie)
}

const TOR_SECRET_KEY_LENGTH: usize = 64;

fn load_or_create_tor_secret_key(path: &str) -> Result<TorSecretKeyV3, String> {
    if Path::new(path).exists() {
        load_tor_secret_key(path)
    } else {
        create_tor_secret_key(path)
    }
}

fn create_tor_secret_key(path: &str) -> Result<TorSecretKeyV3, String> {
    let key = TorSecretKeyV3::generate();
    info!(
        "Generated new onion service v3 key for address: {}",
        key.public().get_onion_address()
    );

    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut file_options = OpenOptions::new();
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = file_options.create(true).truncate(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options = options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|err| format!("Failed to open onion private key for writing: {:?}", err))?;
    file.write_all(
        &base64::engine::general_purpose::STANDARD
            .encode(key.as_bytes())
            .into_bytes(),
    )
    .map_err(|err| format!("Failed to write onion private key: {:?}", err))?;

    Ok(key)
}

fn load_tor_secret_key(path: &str) -> Result<TorSecretKeyV3, String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(
            std::fs::read_to_string(path)
                .map_err(|err| format!("Read onion private key ({}) failed: {}", path, err))?,
        )
        .map_err(|err| format!("Failed to decode onion private key: {:?}", err))?;
    if raw.len() != TOR_SECRET_KEY_LENGTH {
        return Err(format!(
            "Invalid secret key length: expected {}, got {}",
            TOR_SECRET_KEY_LENGTH,
            raw.len()
        ));
    }
    let mut buf = [0u8; TOR_SECRET_KEY_LENGTH];
    buf.copy_from_slice(&raw);
    Ok(TorSecretKeyV3::from(buf))
}
