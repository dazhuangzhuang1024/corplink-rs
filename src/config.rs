use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::Path;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::State;
use crate::utils;

const DEFAULT_DEVICE_NAME: &str = "DollarOS";
const DEFAULT_INTERFACE_NAME: &str = "corplink";

pub const PLATFORM_LDAP: &str = "ldap";
pub const PLATFORM_CORPLINK: &str = "feilian";
// new feilian login that uses the v1 API (/api/v1/login with an AES-encrypted
// password), as served by the newer feilian backend. opt-in via config.
pub const PLATFORM_CORPLINK_V1: &str = "feilian_v1";
pub const PLATFORM_OIDC: &str = "OIDC";
// aka feishu
pub const PLATFORM_LARK: &str = "lark";
#[allow(dead_code)]
pub const PLATFORM_WEIXIN: &str = "weixin";
// aka dingding
#[allow(dead_code)]
pub const PLATFORM_DING_TALK: &str = "dingtalk";
// unknown
#[allow(dead_code)]
pub const PLATFORM_AAD: &str = "aad";

pub const STRATEGY_LATENCY: &str = "latency";
pub const STRATEGY_DEFAULT: &str = "default";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RouteMode {
    /// Only intranet routes returned by the server (mimics official split mode).
    #[default]
    Split,
    /// Full-tunnel routes from the server (typically 0.0.0.0/0, ::/0).
    Full,
}

impl fmt::Display for RouteMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RouteMode::Split => write!(f, "split"),
            RouteMode::Full => write!(f, "full"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub company_name: String,
    pub username: String,
    pub password: Option<String>,
    pub platform: Option<String>,
    pub code: Option<String>,
    pub device_name: Option<String>,
    pub device_id: Option<String>,
    pub public_key: Option<String>,
    pub private_key: Option<String>,
    pub server: Option<String>,
    pub interface_name: Option<String>,
    pub debug_wg: Option<bool>,
    #[serde(skip_serializing)]
    pub conf_file: Option<String>,
    pub state: Option<State>,
    pub vpn_server_name: Option<String>,
    /// Runtime-only override for `vpn_server_name`, currently fed by the
    /// CORPLINK_VPN_SERVER_NAME environment variable so a launchd job can pin a
    /// node without editing the config file. Skipped by serde in both directions:
    /// it cannot be set from the config file, and `save` can never write it back.
    /// Read it through [`Config::effective_vpn_server_name`], never directly.
    #[serde(skip)]
    pub vpn_server_name_override: Option<String>,
    pub vpn_select_strategy: Option<String>,
    pub use_vpn_dns: Option<bool>,
    pub dns_backup_filename: Option<String>,
    /// macOS only: domains that should resolve via the VPN-assigned DNS server.
    /// On every successful connection the daemon writes `/etc/resolver/<domain>`
    /// files pointing at the DNS assigned for that session (which can change
    /// between sessions), and removes them again on graceful shutdown. Only
    /// files carrying corplink-rs's marker line are ever modified; marked files
    /// whose interface no longer exists, e.g. left behind by a killed run, are
    /// removed at the next start. Needs `interface_name` of the form utunN.
    /// Unlike `use_vpn_dns`, the system-wide DNS settings are left untouched:
    /// only the listed domains (e.g. "intranet.example.com") go through the
    /// VPN. Include CNAME target domains -- macOS resolves the follow-up query
    /// with the resolver matching the target domain, which falls back to the
    /// default DNS if it is not listed.
    pub split_dns_domains: Option<Vec<String>>,
    pub auto_setup_routes: Option<bool>,
    /// "split" (default) or "full". Selects which route list from the server to apply.
    pub route_mode: Option<RouteMode>,
    /// Optional CIDRs added to the server-provided routes before route filters.
    /// Unlike `vpn_allowed_routes`, this expands the route set. The combined routes
    /// are then restricted by `vpn_allowed_routes` and `vpn_disallowed_routes`.
    pub vpn_additional_routes: Option<Vec<String>>,
    /// Optional hostnames resolved on every connection. Resolved addresses are appended
    /// as host routes before route filters.
    pub vpn_additional_domains: Option<Vec<String>>,
    /// Optional CIDR whitelist intersected with the server and additional routes.
    /// Missing/null preserves the combined routes; an empty list allows no routes.
    pub vpn_allowed_routes: Option<Vec<String>>,
    /// Optional list of CIDR routes to exclude from AllowedIPs / system routes.
    /// Useful in full mode to punch holes for local LAN or the VPN peer IP itself,
    /// avoiding routing loops (e.g. 192.168.1.0/24, 10.0.0.5/32).
    pub vpn_disallowed_routes: Option<Vec<String>>,
    /// When set, run entirely in userspace (gVisor netstack) and expose a SOCKS5
    /// proxy at this listen address (e.g. "0.0.0.0:1080" or "127.0.0.1:1080")
    /// instead of creating a kernel TUN device. No system interface, routes, DNS
    /// changes or root privileges are required. Only TCP CONNECT is supported.
    pub socks5_listen: Option<String>,
    /// Optional SOCKS5 username/password authentication (RFC 1929). When
    /// `socks5_username` is set and non-empty, clients must authenticate with
    /// these credentials; otherwise the proxy accepts connections without auth.
    pub socks5_username: Option<String>,
    pub socks5_password: Option<String>,
    /// Force the WireGuard transport protocol instead of using the server-advertised
    /// `protocol_mode`. Accepts "udp" or "tcp" (case-insensitive). Some `protocol_mode: 1`
    /// (TCP) gateways also accept WireGuard over UDP -- for those the server even ships a
    /// `protocol_detect_config` (udp<->tcp switch thresholds) in the `/api/vpn/list` entry.
    /// Since WireGuard-over-TCP can collapse to a few KB/s on a lossy uplink (TCP-over-TCP
    /// head-of-line blocking), forcing "udp" can be far faster there. Leave unset to keep the
    /// default (follow server `protocol_mode`: 1 => tcp, otherwise udp).
    pub force_protocol: Option<String>,
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match serde_json::to_string_pretty(self) {
            Ok(s) => write!(f, "{}", s),
            Err(e) => write!(f, "<invalid config: {e}>"),
        }
    }
}

impl Config {
    pub async fn from_file(file: &str) -> Result<Config> {
        // `save` refuses to write through a symlink; say so now rather than at
        // the first save, which may only come hours later with a logout
        let meta = fs::symlink_metadata(file)
            .await
            .with_context(|| format!("failed to read config file {file}"))?;
        if meta.file_type().is_symlink() {
            bail!("config file {file} is a symlink, which corplink-rs refuses to write back; use the real file");
        }
        let conf_str = fs::read_to_string(file)
            .await
            .with_context(|| format!("failed to read config file {file}"))?;

        let mut conf: Config = serde_json::from_str(&conf_str[..])
            .with_context(|| format!("failed to parse config file {file}"))?;

        conf.conf_file = Some(file.to_string());
        let mut update_conf = false;
        if conf.interface_name.is_none() {
            conf.interface_name = Some(DEFAULT_INTERFACE_NAME.to_string());
            update_conf = true;
        }
        if conf.device_name.is_none() {
            conf.device_name = Some(DEFAULT_DEVICE_NAME.to_string());
            update_conf = true;
        }
        if conf.device_id.is_none() {
            let device_name = conf
                .device_name
                .as_ref()
                .context("device name missing when generating device id")?;
            conf.device_id = Some(format!("{:x}", md5::compute(device_name)));
            update_conf = true;
        }
        match &conf.private_key {
            Some(private_key) => match conf.public_key {
                Some(_) => {
                    // both keys exist, do nothing
                }
                None => {
                    // only private key exists, generate public from private
                    let public_key = utils::gen_public_key_from_private(private_key)?;
                    conf.public_key = Some(public_key);
                    update_conf = true;
                }
            },
            None => {
                // no key exists, generate new
                let (public_key, private_key) = utils::gen_wg_keypair();
                (conf.public_key, conf.private_key) = (Some(public_key), Some(private_key));
                update_conf = true;
            }
        }
        if update_conf {
            conf.save().await?;
        }
        Ok(conf)
    }

    /// VPN server name to filter on: the runtime override wins over the value
    /// persisted in the config file.
    pub fn effective_vpn_server_name(&self) -> Option<&str> {
        self.vpn_server_name_override
            .as_deref()
            .or(self.vpn_server_name.as_deref())
    }

    /// The config holds the WireGuard private key and the login state, so it
    /// is written to a temporary file next to it and renamed into place: a
    /// crash or a full disk can never leave it truncated. Where the directory
    /// cannot take that (not writable, a single-file bind mount), the file is
    /// rewritten in place as before. A symlinked config file is refused, the
    /// daemon writes it as root.
    pub async fn save(&self) -> Result<()> {
        let file = self
            .conf_file
            .as_ref()
            .context("config file path missing")?;
        let data = format!("{}", &self);
        let path = Path::new(file);

        let meta = fs::symlink_metadata(path)
            .await
            .with_context(|| format!("failed to inspect config file {file}"))?;
        if meta.file_type().is_symlink() {
            bail!("refusing to write config file {file} through a symlink");
        }
        let mut tmp_name = OsString::from(".");
        tmp_name.push(
            path.file_name()
                .context("config file path has no file name")?,
        );
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);

        match replace_file(path, &tmp, &meta, data.as_bytes()).await {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied
                        | io::ErrorKind::ReadOnlyFilesystem
                        | io::ErrorKind::ResourceBusy
                        | io::ErrorKind::CrossesDevices
                ) =>
            {
                log::warn!(
                    "cannot replace config file {file} atomically ({e}), rewriting it in place"
                );
                rewrite_file(path, data.as_bytes())
                    .await
                    .with_context(|| format!("failed to write config file {file}"))
            }
            Err(e) => Err(e).with_context(|| format!("failed to write config file {file}")),
        }
    }
}

// writes `tmp` next to `path` and renames it over `path`
async fn replace_file(
    path: &Path,
    tmp: &Path,
    meta: &std::fs::Metadata,
    data: &[u8],
) -> io::Result<()> {
    // left behind by a save that crashed; unlink never follows a symlink
    match fs::remove_file(tmp).await {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let result = async {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        // private until the original permissions are copied over below
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        let mut output = options.open(tmp).await?;
        #[cfg(unix)]
        output.set_permissions(meta.permissions()).await?;
        output.write_all(data).await?;
        output.sync_all().await?;
        drop(output);
        // keep the file its owner's when the daemon saves it as root
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Err(e) = std::os::unix::fs::lchown(tmp, Some(meta.uid()), Some(meta.gid())) {
                log::warn!("failed to keep the owner of {}: {e}", path.display());
            }
        }
        fs::rename(tmp, path).await
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(tmp).await;
    }
    result
}

async fn rewrite_file(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut output = options.open(path).await?;
    output.write_all(data).await?;
    output.sync_all().await
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::Config;

    fn minimal_config() -> Config {
        serde_json::from_str(r#"{"company_name":"test","username":"test"}"#).unwrap()
    }

    #[test]
    fn runtime_override_wins_over_the_configured_server_name() {
        let mut config = minimal_config();
        assert_eq!(config.effective_vpn_server_name(), None);

        config.vpn_server_name = Some("from-file".to_string());
        assert_eq!(config.effective_vpn_server_name(), Some("from-file"));

        config.vpn_server_name_override = Some("from-env".to_string());
        assert_eq!(config.effective_vpn_server_name(), Some("from-env"));

        config.vpn_server_name = None;
        assert_eq!(config.effective_vpn_server_name(), Some("from-env"));
    }

    #[test]
    fn runtime_override_is_never_serialized_and_never_read_from_file() {
        let mut config = minimal_config();
        config.vpn_server_name = Some("from-file".to_string());
        config.vpn_server_name_override = Some("from-env".to_string());

        // `save` serializes via Display, so this is exactly what hits the file.
        let serialized = format!("{config}");
        assert!(!serialized.contains("vpn_server_name_override"));
        assert!(!serialized.contains("from-env"));
        assert!(serialized.contains("from-file"));

        // Reloading the written config must not resurrect the override.
        let reloaded: Config = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reloaded.vpn_server_name_override, None);
        assert_eq!(reloaded.effective_vpn_server_name(), Some("from-file"));

        // The field is also not settable from a hand-written config file.
        let injected: Config = serde_json::from_str(
            r#"{"company_name":"test","username":"test","vpn_server_name_override":"sneaky"}"#,
        )
        .unwrap();
        assert_eq!(injected.vpn_server_name_override, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_save_refuses_symlink() {
        use std::os::unix::fs::symlink;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target = std::env::temp_dir().join(format!("corplink-config-target-{unique}"));
        let link = std::env::temp_dir().join(format!("corplink-config-link-{unique}"));
        std::fs::write(&target, b"unchanged").unwrap();
        symlink(&target, &link).unwrap();

        let mut config = minimal_config();
        config.conf_file = Some(link.to_string_lossy().into_owned());

        assert!(config.save().await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");

        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(&target).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_save_replaces_the_file_and_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("corplink-config-save-{unique}"));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            b"old content that is longer than the new one, padding padding",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        // a leftover of a save that crashed
        std::fs::write(dir.join(".config.json.tmp"), b"stale").unwrap();

        let mut config = minimal_config();
        config.conf_file = Some(path.to_string_lossy().into_owned());
        config.vpn_server_name = Some("saved".to_string());
        config.save().await.unwrap();

        let reloaded: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.vpn_server_name.as_deref(), Some("saved"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(entries, ["config.json"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_config_file_is_refused_on_load() {
        use std::os::unix::fs::symlink;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target = std::env::temp_dir().join(format!("corplink-config-load-target-{unique}"));
        let link = std::env::temp_dir().join(format!("corplink-config-load-link-{unique}"));
        // complete, so that loading it would not save it
        let mut complete = minimal_config();
        complete.interface_name = Some("utun1".to_string());
        complete.device_name = Some("device".to_string());
        complete.device_id = Some("id".to_string());
        complete.private_key = Some("private".to_string());
        complete.public_key = Some("public".to_string());
        let content = format!("{complete}");
        std::fs::write(&target, &content).unwrap();
        symlink(&target, &link).unwrap();

        let result = Config::from_file(&link.to_string_lossy()).await;
        std::fs::remove_file(&link).unwrap();
        let unchanged = std::fs::read_to_string(&target).unwrap() == content;
        std::fs::remove_file(&target).unwrap();
        assert!(format!("{:#}", result.err().unwrap()).contains("is a symlink"));
        assert!(unchanged);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_save_rewrites_in_place_where_it_cannot_rename() {
        use std::os::unix::fs::PermissionsExt;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("corplink-config-readonly-{unique}"));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let mut config = minimal_config();
        config.conf_file = Some(path.to_string_lossy().into_owned());
        config.vpn_server_name = Some("saved".to_string());
        let result = config.save().await;
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        result.unwrap();
        assert!(content.contains("saved"));
    }
}

#[derive(Serialize, Clone)]
pub struct WgConf {
    // standard wg conf
    pub address: String,
    pub address6: String,
    pub peer_address: String,
    pub mtu: u32,
    pub public_key: String,
    pub private_key: String,
    pub peer_key: String,
    pub allowed_ips: Vec<String>,
    pub routes: Vec<String>,

    // extra confs
    pub dns: String,

    // corplink confs
    pub protocol: i32,
}
