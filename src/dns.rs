use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
pub use resolver::{interface_exists, split_dns_servers, ResolverFiles, RESOLVER_DIR};

#[cfg(target_os = "linux")]
const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";

#[cfg(target_os = "linux")]
const LINUX_DEFAULT_BACKUP_FILENAME: &str = "resolv.conf.corplink";

pub struct DNSManager {
    #[cfg(target_os = "macos")]
    service_dns: HashMap<String, String>,
    #[cfg(target_os = "macos")]
    service_dns_search: HashMap<String, String>,

    #[cfg(target_os = "linux")]
    backup_path: PathBuf,
}

impl DNSManager {
    pub fn new(_backup_filename: Option<String>) -> DNSManager {
        DNSManager {
            #[cfg(target_os = "macos")]
            service_dns: HashMap::new(),
            #[cfg(target_os = "macos")]
            service_dns_search: HashMap::new(),

            #[cfg(target_os = "linux")]
            backup_path: {
                let filename = _backup_filename
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| LINUX_DEFAULT_BACKUP_FILENAME.to_string());
                Path::new(RESOLV_CONF_PATH)
                    .parent()
                    .unwrap_or_else(|| Path::new("/etc"))
                    .join(filename)
            },
        }
    }

    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    pub fn backup_path(&self) -> &Path {
        &self.backup_path
    }
}

/// macOS per-domain split DNS through /etc/resolver files: the system-wide
/// DNS settings stay untouched (unlike `DNSManager::set_dns`), only the listed
/// domains are resolved by the DNS server assigned for this VPN session.
#[cfg(any(target_os = "macos", test))]
mod resolver {
    use anyhow::{bail, Context, Result};
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::IpAddr;
    use std::path::{Path, PathBuf};

    /// macOS reads per-domain resolver configs from here, the file name being
    /// the domain.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub const RESOLVER_DIR: &str = "/etc/resolver";

    const MARKER_PREFIX: &str = "# managed by corplink-rs for interface ";
    // how much of a file is read back to find its marker line
    const MARKER_READ_LIMIT: usize = 256;

    /// Every file starts with a marker line naming the owning interface, so an
    /// instance only ever overwrites or removes files it wrote itself:
    /// hand-written files, those of other tools and those of another live
    /// corplink-rs instance are left alone. The marker also lets the next start
    /// clean up files that a killed or crashed run could not remove.
    pub struct ResolverFiles {
        base: PathBuf,
        interface_name: String,
        marker: String,
        // files written by this process, removed again on shutdown
        written: Vec<PathBuf>,
    }

    impl ResolverFiles {
        pub fn new(base: impl Into<PathBuf>, interface_name: &str) -> ResolverFiles {
            ResolverFiles {
                base: base.into(),
                interface_name: interface_name.to_string(),
                // Debug formatting escapes control characters, so no interface
                // name can break out of the comment line
                marker: format!("{MARKER_PREFIX}{interface_name:?}\n"),
                written: Vec::new(),
            }
        }

        /// Remove files left behind by runs that did not exit gracefully
        /// (SIGKILL, crash, power loss): this interface's, and those of any
        /// other corplink-rs interface, e.g. one renamed since. Without the VPN
        /// their DNS server is unreachable, which breaks the listed domains even
        /// where they also resolve publicly. A file whose interface still exists
        /// belongs to a live instance and is kept: the utun device lives exactly
        /// as long as the process holding it, even one killed by SIGKILL.
        pub fn remove_stale(&self, interface_exists: impl Fn(&str) -> bool) -> Result<()> {
            match fs::symlink_metadata(&self.base) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                // quietly: this runs on every start, split dns or not, and
                // `write` warns about it where split dns is configured
                _ => {
                    if let Err(e) = self.check_base() {
                        log::debug!("{e:#}");
                        return Ok(());
                    }
                }
            }
            let entries = fs::read_dir(&self.base)
                .with_context(|| format!("failed to list {}", self.base.display()))?;
            let mut failures = Vec::new();
            for entry in entries {
                let path = match entry {
                    Ok(entry) => entry.path(),
                    Err(e) => {
                        failures.push(format!("{}: {e}", self.base.display()));
                        continue;
                    }
                };
                let stale = match self.first_line(&path) {
                    Ok(line) => line
                        .as_deref()
                        .and_then(marker_owner)
                        .is_some_and(|owner| !interface_exists(owner)),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                    Err(e) => {
                        failures.push(format!("{}: {e}", path.display()));
                        false
                    }
                };
                if stale {
                    match fs::remove_file(&path) {
                        Ok(()) => log::info!("split dns: removed stale {}", path.display()),
                        Err(e) => failures.push(format!("{}: {e}", path.display())),
                    }
                }
            }
            check_failures("failed to remove stale resolver files", failures)
        }

        /// Point every domain at the DNS servers assigned for this session. The
        /// assigned DNS can change between sessions, which is exactly why the
        /// files are rewritten on every connect. Unusable entries are skipped
        /// one by one instead of disabling split DNS altogether.
        pub fn write(&mut self, dns_servers: &str, domains: &[String]) -> Result<()> {
            // the marker must name the real device for the liveness check in
            // `remove_stale`
            if !is_canonical_utun(&self.interface_name) {
                log::warn!(
                    "split dns: needs interface_name of the form utunN (e.g. utun12345), \
                     got {:?}; no resolver files written",
                    self.interface_name
                );
                return Ok(());
            }

            let mut servers: Vec<IpAddr> = Vec::new();
            for server in split_dns_servers(dns_servers) {
                match server.parse() {
                    Ok(ip) => servers.push(ip),
                    Err(_) => log::warn!("split dns: ignoring DNS server {server:?}, not an IP"),
                }
            }
            if servers.is_empty() {
                log::warn!(
                    "split dns: no DNS server IP in the VPN-assigned {dns_servers:?}, \
                     no resolver files written"
                );
                return Ok(());
            }

            let mut normalized: Vec<String> = Vec::new();
            for domain in domains {
                match normalize_domain(domain) {
                    Some(domain) if normalized.contains(&domain) => {}
                    Some(domain) => normalized.push(domain),
                    None => log::warn!("split dns: ignoring invalid domain {domain:?}"),
                }
            }
            if normalized.is_empty() {
                return Ok(());
            }

            create_dir(&self.base).with_context(|| {
                format!(
                    "failed to create resolver directory {}",
                    self.base.display()
                )
            })?;
            self.check_base()?;
            let content = self.render(&servers);
            let servers = servers
                .iter()
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
                .join(", ");

            let mut failures = Vec::new();
            for domain in normalized {
                let path = self.base.join(&domain);
                match self.first_line(&path) {
                    Ok(Some(line)) if line == self.marker.as_bytes() => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Ok(line) => {
                        match line.as_deref().and_then(marker_owner) {
                            Some(owner) => log::warn!(
                                "split dns: {} belongs to corplink-rs on {owner}, which is \
                                 still up, leaving it alone",
                                path.display()
                            ),
                            None => log::warn!(
                                "split dns: {} was not written by corplink-rs, leaving it alone",
                                path.display()
                            ),
                        }
                        continue;
                    }
                    Err(e) => {
                        failures.push(format!("{}: {e}", path.display()));
                        continue;
                    }
                }
                match open_for_write(&path) {
                    Ok(mut file) => match file.write_all(content.as_bytes()) {
                        Ok(()) => {
                            log::info!("split dns: {} -> {}", path.display(), servers);
                            if !self.written.contains(&path) {
                                self.written.push(path);
                            }
                        }
                        Err(e) => {
                            // a half-written file lacks the complete marker line,
                            // so nothing would ever rewrite or remove it again
                            let _ = fs::remove_file(&path);
                            self.written.retain(|written| written != &path);
                            failures.push(format!("{}: {e}", path.display()));
                        }
                    },
                    Err(e) => failures.push(format!("{}: {e}", path.display())),
                }
            }
            check_failures("failed to write resolver files", failures)
        }

        /// Remove the files written by `write`, unless something else has
        /// replaced them since. While the VPN is down the assigned DNS is
        /// unreachable, so leaving them behind would only break resolution.
        pub fn remove(&mut self) -> Result<()> {
            let mut failures = Vec::new();
            for path in std::mem::take(&mut self.written) {
                match self.is_managed(&path) {
                    Ok(true) => match fs::remove_file(&path) {
                        Ok(()) => log::info!("split dns: removed {}", path.display()),
                        Err(e) => failures.push(format!("{}: {e}", path.display())),
                    },
                    Ok(false) => log::warn!(
                        "split dns: {} was replaced by something else, leaving it alone",
                        path.display()
                    ),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => failures.push(format!("{}: {e}", path.display())),
                }
            }
            check_failures("failed to remove resolver files", failures)
        }

        /// The marker line, then one `nameserver` line per server.
        fn render(&self, dns_servers: &[IpAddr]) -> String {
            let mut content = self.marker.clone();
            for server in dns_servers {
                content.push_str(&format!("nameserver {server}\n"));
            }
            content
        }

        /// Root may only create, rewrite and remove files in a real directory
        /// (not a symlink) that nobody else can write to; otherwise another user
        /// could swap entries for hard links or fifos between the checks and
        /// the opens below. The owner check is against the effective user,
        /// which is root for the daemon.
        fn check_base(&self) -> Result<()> {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let meta = fs::symlink_metadata(&self.base)
                    .with_context(|| format!("failed to inspect {}", self.base.display()))?;
                // SAFETY: geteuid has no preconditions and cannot fail
                let euid = unsafe { libc::geteuid() };
                if !meta.file_type().is_dir() || meta.uid() != euid || meta.mode() & 0o022 != 0 {
                    bail!(
                        "split dns: {} is not a directory owned by uid {euid} that only it can \
                         write to, leaving it alone",
                        self.base.display()
                    );
                }
            }
            Ok(())
        }

        /// Whether `path` is a regular file (not a symlink) starting with this
        /// instance's marker line. NotFound is passed through.
        fn is_managed(&self, path: &Path) -> io::Result<bool> {
            Ok(self.first_line(path)?.as_deref() == Some(self.marker.as_bytes()))
        }

        /// The first line of a regular file (not a symlink), including its
        /// newline, or None when there is no newline within the bytes read.
        /// NotFound is passed through.
        fn first_line(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            if !fs::symlink_metadata(path)?.file_type().is_file() {
                return Ok(None);
            }
            let limit = MARKER_READ_LIMIT.max(self.marker.len());
            let mut head = Vec::with_capacity(limit);
            no_follow_options()
                .read(true)
                .open(path)?
                .take(limit as u64)
                .read_to_end(&mut head)?;
            Ok(head
                .iter()
                .position(|&b| b == b'\n')
                .map(|end| head[..=end].to_vec()))
        }
    }

    /// Whether `name` is exactly the kernel name of a numbered utun device.
    /// wireguard-go also accepts "utun" (the kernel picks the unit) and
    /// spellings like "utun012" (which becomes utun12).
    pub(super) fn is_canonical_utun(name: &str) -> bool {
        name.strip_prefix("utun").is_some_and(|unit| {
            !unit.is_empty()
                && unit.bytes().all(|b| b.is_ascii_digit())
                && (unit == "0" || !unit.starts_with('0'))
        })
    }

    /// The interface named in a corplink-rs marker line. Names that needed
    /// escaping are not parsed back, so their files are never treated as stale.
    fn marker_owner(line: &[u8]) -> Option<&str> {
        let quoted = std::str::from_utf8(line)
            .ok()?
            .strip_prefix(MARKER_PREFIX)?
            .strip_suffix('\n')?;
        let name = quoted.strip_prefix('"')?.strip_suffix('"')?;
        (!name.is_empty() && !name.contains(['"', '\\'])).then_some(name)
    }

    /// The servers in the VPN-assigned DNS setting, separated by commas or
    /// whitespace.
    pub fn split_dns_servers(dns_servers: &str) -> Vec<&str> {
        dns_servers
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Whether a network interface with this name exists right now.
    #[cfg(target_os = "macos")]
    pub fn interface_exists(name: &str) -> bool {
        match std::ffi::CString::new(name) {
            // SAFETY: a valid NUL-terminated string that outlives the call
            Ok(name) => unsafe { libc::if_nametoindex(name.as_ptr()) != 0 },
            Err(_) => false,
        }
    }

    /// Trim, lowercase and drop one trailing dot, then require a plain DNS
    /// name. It becomes a file name under /etc/resolver and the daemon runs as
    /// root, so only non-empty labels of ascii letters, digits and hyphens are
    /// accepted: no path separators, no `.`/`..`, no traversal.
    pub(super) fn normalize_domain(domain: &str) -> Option<String> {
        let domain = domain.trim().to_ascii_lowercase();
        let domain = domain.strip_suffix('.').unwrap_or(&domain);
        let valid = domain.len() <= 253
            && domain.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        valid.then(|| domain.to_string())
    }

    // never follow a symlink planted at the target, the daemon runs as root;
    // never block on a fifo either
    fn no_follow_options() -> fs::OpenOptions {
        #[allow(unused_mut)]
        let mut options = fs::OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        options
    }

    pub(super) fn open_for_write(path: &Path) -> io::Result<fs::File> {
        let mut options = no_follow_options();
        options.write(true).create(true).truncate(true);
        // at most 0644, the umask can only narrow it
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        options.open(path)
    }

    // at most 0755 (the umask can only narrow it), so `check_base` accepts it
    pub(super) fn create_dir(path: &Path) -> io::Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o755);
        }
        builder.create(path)
    }

    // every file is attempted, then all failures are reported together
    fn check_failures(what: &str, failures: Vec<String>) -> Result<()> {
        if failures.is_empty() {
            return Ok(());
        }
        bail!("{what}: {}", failures.join("; "))
    }
}

#[cfg(target_os = "macos")]
impl DNSManager {
    fn collect_new_service_dns(&mut self) -> Result<()> {
        let output = Command::new("networksetup")
            .arg("-listallnetworkservices")
            .output()
            .context("failed to list network services")?;

        let services = String::from_utf8_lossy(&output.stdout);
        let lines = services.lines();
        // Skip the first line's legend
        for service in lines.skip(1) {
            // Remove leading '*' and trim whitespace
            let service = service.trim_start_matches('*').trim();
            if service.is_empty() {
                continue;
            }

            // get DNS servers
            let dns_output = Command::new("networksetup")
                .arg("-getdnsservers")
                .arg(service)
                .output()
                .with_context(|| format!("failed to get dns servers for {service}"))?;
            let dns_response = String::from_utf8_lossy(&dns_output.stdout)
                .trim()
                .to_string();
            // if dns config for this service is not empty, output should be ip addresses seperated in lines without space
            // otherwise, output should be "There aren't any DNS Servers set on xxx", use "Empty" instead, which can be recognized in 'networksetup -setdnsservers'
            let dns_response = if dns_response.contains(" ") {
                "Empty".to_string()
            } else {
                dns_response
            };

            self.service_dns
                .insert(service.to_string(), dns_response.clone());

            // get search domain
            let search_output = Command::new("networksetup")
                .arg("-getsearchdomains")
                .arg(service)
                .output()
                .with_context(|| format!("failed to get search domains for {service}"))?;
            let search_response = String::from_utf8_lossy(&search_output.stdout)
                .trim()
                .to_string();
            let search_response = if search_response.contains(" ") {
                "Empty".to_string()
            } else {
                search_response
            };

            self.service_dns_search
                .insert(service.to_string(), search_response.clone());

            log::debug!(
                "DNS collected for {}, dns servers: {}, search domain: {}",
                service,
                dns_response,
                search_response
            )
        }
        Ok(())
    }

    pub fn set_dns(&mut self, dns_servers: Vec<&str>, dns_search: Vec<&str>) -> Result<()> {
        if dns_servers.is_empty() {
            return Ok(());
        }
        self.collect_new_service_dns()?;
        for service in self.service_dns.keys() {
            Command::new("networksetup")
                .arg("-setdnsservers")
                .arg(service)
                .args(&dns_servers)
                .status()
                .with_context(|| format!("failed to set dns servers for {service}"))?;

            if !dns_search.is_empty() {
                Command::new("networksetup")
                    .arg("-setsearchdomains")
                    .arg(service)
                    .args(&dns_search)
                    .status()
                    .with_context(|| format!("failed to set search domains for {service}"))?;
            }
            log::debug!("DNS set for {} with {}", service, dns_servers.join(","));
        }

        Ok(())
    }

    pub fn restore_dns(&self) -> Result<()> {
        for (service, dns) in &self.service_dns {
            Command::new("networksetup")
                .arg("-setdnsservers")
                .arg(service)
                .args(dns.lines())
                .status()
                .with_context(|| format!("failed to reset dns servers for {service}"))?;

            log::debug!("DNS server reset for {} with {}", service, dns);
        }
        for (service, search_domain) in &self.service_dns_search {
            Command::new("networksetup")
                .arg("-setsearchdomains")
                .arg(service)
                .args(search_domain.lines())
                .status()
                .with_context(|| format!("failed to reset search domains for {service}"))?;
            log::debug!(
                "DNS search domain reset for {} with {}",
                service,
                search_domain
            )
        }
        log::debug!("DNS reset");
        Ok(())
    }
}

#[cfg(test)]
mod resolver_tests {
    use super::resolver::{create_dir, is_canonical_utun, normalize_domain, ResolverFiles};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    // a scratch resolver directory, removed even when an assertion fails
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            TempDir(std::env::temp_dir().join(format!("corplink-{name}-{unique}")))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn domains(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn domain_normalization() {
        for (input, expected) in [
            ("intranet.example.com", "intranet.example.com"),
            ("Intranet.Example.COM.", "intranet.example.com"),
            ("  intranet.example.com\t", "intranet.example.com"),
            ("a-b", "a-b"),
            ("x.1.y", "x.1.y"),
        ] {
            assert_eq!(
                normalize_domain(input).as_deref(),
                Some(expected),
                "{input:?}"
            );
        }
        let label63 = "a".repeat(63);
        assert!(normalize_domain(&label63).is_some());
        assert!(normalize_domain(&format!("{label63}a")).is_none());
        let long = vec!["a".repeat(50); 5].join(".");
        assert!(long.len() > 253);
        assert!(normalize_domain(&long).is_none());
        for domain in [
            "",
            ".",
            "..",
            ".a",
            "a..",
            "a..b",
            "a/b",
            "../etc/passwd",
            "a b",
            "a\\b",
            "a_b",
        ] {
            assert_eq!(
                normalize_domain(domain),
                None,
                "{domain:?} should be invalid"
            );
        }
    }

    #[test]
    fn write_then_remove() {
        let dir = TempDir::new("resolver-write");
        let mut files = ResolverFiles::new(&dir.0, "utun1");
        let marker = "# managed by corplink-rs for interface \"utun1\"\n";

        files
            .write(
                "10.2.2.17",
                &domains(&["intranet.example.com", "Sub.Example.com."]),
            )
            .unwrap();
        for domain in ["intranet.example.com", "sub.example.com"] {
            assert_eq!(
                read(&dir.0.join(domain)),
                format!("{marker}nameserver 10.2.2.17\n")
            );
        }

        // a new session's DNS replaces the old one; comma/space lists give one
        // nameserver line each
        files
            .write("10.2.2.18, 10.3.3.19", &domains(&["intranet.example.com"]))
            .unwrap();
        assert_eq!(
            read(&dir.0.join("intranet.example.com")),
            format!("{marker}nameserver 10.2.2.18\nnameserver 10.3.3.19\n")
        );

        files.remove().unwrap();
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
        // nothing left to remove
        files.remove().unwrap();
    }

    #[test]
    fn invalid_entries_are_skipped_one_by_one() {
        let dir = TempDir::new("resolver-invalid");
        let mut files = ResolverFiles::new(dir.0.join("resolver"), "utun1");

        files
            .write(
                "10.2.2.17",
                &domains(&["../evil", "intranet.example.com", "INTRANET.example.com."]),
            )
            .unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.0.join("resolver"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["intranet.example.com"]);
        assert!(!dir.0.join("evil").exists());
    }

    #[test]
    fn nothing_is_written_without_servers_or_domains() {
        let dir = TempDir::new("resolver-empty");
        let mut files = ResolverFiles::new(&dir.0, "utun1");
        files
            .write("", &domains(&["intranet.example.com"]))
            .unwrap();
        files.write("10.2.2.17", &[]).unwrap();
        files.write("10.2.2.17", &domains(&["a/b"])).unwrap();
        assert!(!dir.0.exists());
    }

    #[test]
    fn foreign_files_are_never_overwritten_or_removed() {
        let dir = TempDir::new("resolver-foreign");
        create_dir(&dir.0).unwrap();
        let hand_written = dir.0.join("intranet.example.com");
        std::fs::write(&hand_written, "nameserver 1.2.3.4\nport 5353\n").unwrap();

        let mut other = ResolverFiles::new(&dir.0, "utun2");
        other
            .write("10.9.9.9", &domains(&["shared.example.com"]))
            .unwrap();
        let other_file = read(&dir.0.join("shared.example.com"));

        let mut files = ResolverFiles::new(&dir.0, "utun1");
        let listed = domains(&[
            "intranet.example.com",
            "shared.example.com",
            "ok.example.com",
        ]);
        // the skipped entries are only warned about
        files.write("10.2.2.17", &listed).unwrap();
        assert!(read(&dir.0.join("ok.example.com"))
            .starts_with("# managed by corplink-rs for interface \"utun1\"\n"));
        assert_eq!(read(&hand_written), "nameserver 1.2.3.4\nport 5353\n");
        assert_eq!(read(&dir.0.join("shared.example.com")), other_file);

        files.remove().unwrap();
        assert!(!dir.0.join("ok.example.com").exists());
        // utun2 is a live instance
        files.remove_stale(|name| name == "utun2").unwrap();
        assert_eq!(read(&hand_written), "nameserver 1.2.3.4\nport 5353\n");
        assert_eq!(read(&dir.0.join("shared.example.com")), other_file);
    }

    #[test]
    fn a_file_replaced_after_writing_is_not_removed() {
        let dir = TempDir::new("resolver-replaced");
        let mut files = ResolverFiles::new(&dir.0, "utun1");
        files
            .write("10.2.2.17", &domains(&["intranet.example.com"]))
            .unwrap();

        let path = dir.0.join("intranet.example.com");
        std::fs::write(&path, "nameserver 1.2.3.4\n").unwrap();
        files.remove().unwrap();
        assert_eq!(read(&path), "nameserver 1.2.3.4\n");
    }

    #[test]
    fn files_of_dead_instances_are_removed_on_the_next_start() {
        let dir = TempDir::new("resolver-stale");
        // runs that never got to `remove`: this interface, a renamed one and a
        // live instance
        for (interface, domain) in [
            ("utun1", "a.example.com"),
            ("utun1", "b.example.com"),
            ("utun9", "c.example.com"),
            ("utun2", "d.example.com"),
        ] {
            ResolverFiles::new(&dir.0, interface)
                .write("10.2.2.17", &domains(&[domain]))
                .unwrap();
        }
        for (domain, content) in [
            (
                "e",
                "# managed by corplink-rs for interface \"we\\\"ird\"\n",
            ),
            ("f", "nameserver 1.2.3.4\n"),
            ("g", "# managed by corplink-rs for interface \"utun3\""),
        ] {
            std::fs::write(dir.0.join(format!("{domain}.example.com")), content).unwrap();
        }

        ResolverFiles::new(&dir.0, "utun1")
            .remove_stale(|name| name == "utun2")
            .unwrap();
        assert!(!dir.0.join("a.example.com").exists());
        assert!(!dir.0.join("b.example.com").exists());
        assert!(!dir.0.join("c.example.com").exists());
        // live instance, a name that needed escaping, not ours, no full marker
        for domain in ["d", "e", "f", "g"] {
            assert!(
                dir.0.join(format!("{domain}.example.com")).exists(),
                "{domain}"
            );
        }

        // a missing resolver directory is fine
        ResolverFiles::new(dir.0.join("missing"), "utun1")
            .remove_stale(|_| false)
            .unwrap();
    }

    #[test]
    fn files_of_a_live_instance_with_the_same_interface_are_kept() {
        let dir = TempDir::new("resolver-live");
        ResolverFiles::new(&dir.0, "utun1")
            .write("10.2.2.17", &domains(&["a.example.com"]))
            .unwrap();
        // a second instance started while the first one still holds utun1
        ResolverFiles::new(&dir.0, "utun1")
            .remove_stale(|name| name == "utun1")
            .unwrap();
        assert!(dir.0.join("a.example.com").exists());
    }

    #[test]
    fn only_canonical_utun_names_get_resolver_files() {
        for name in ["utun0", "utun1", "utun12345"] {
            assert!(is_canonical_utun(name), "{name}");
        }
        for name in [
            "utun", "utun01", "utun00", "utun1x", "utun+1", "tun1", "en0", "",
        ] {
            assert!(!is_canonical_utun(name), "{name}");
        }

        // the kernel name of these differs from the configured one, so the
        // liveness check could not protect their files
        let dir = TempDir::new("resolver-noncanonical");
        for name in ["utun", "utun012"] {
            ResolverFiles::new(&dir.0, name)
                .write("10.2.2.17", &domains(&["a.example.com"]))
                .unwrap();
        }
        assert!(!dir.0.exists());
    }

    #[test]
    fn dns_servers_that_are_not_ips_are_skipped() {
        let dir = TempDir::new("resolver-servers");
        let mut files = ResolverFiles::new(&dir.0, "utun1");
        files
            .write(
                "dns.example.com;10.2.2.17 10.2.2.18",
                &domains(&["a.example.com"]),
            )
            .unwrap();
        assert_eq!(
            read(&dir.0.join("a.example.com")),
            "# managed by corplink-rs for interface \"utun1\"\nnameserver 10.2.2.18\n"
        );

        files
            .write("dns.example.com", &domains(&["b.example.com"]))
            .unwrap();
        assert!(!dir.0.join("b.example.com").exists());
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_resolver_directories_are_left_alone() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = TempDir::new("resolver-untrusted");
        let writable = dir.0.join("writable");
        create_dir(&writable).unwrap();
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
        let mut files = ResolverFiles::new(&writable, "utun1");
        std::fs::write(
            writable.join("stale.example.com"),
            "# managed by corplink-rs for interface \"utun1\"\n",
        )
        .unwrap();
        files.remove_stale(|_| false).unwrap();
        assert!(writable.join("stale.example.com").exists());
        assert!(files
            .write("10.2.2.17", &domains(&["a.example.com"]))
            .is_err());
        assert!(!writable.join("a.example.com").exists());

        let real = dir.0.join("real");
        create_dir(&real).unwrap();
        let linked = dir.0.join("linked");
        symlink(&real, &linked).unwrap();
        let mut files = ResolverFiles::new(&linked, "utun1");
        assert!(files
            .write("10.2.2.17", &domains(&["a.example.com"]))
            .is_err());
        assert!(!real.join("a.example.com").exists());
    }

    #[cfg(unix)]
    #[test]
    fn fifos_never_block() {
        use super::resolver::open_for_write;
        use std::os::unix::fs::FileTypeExt;

        let dir = TempDir::new("resolver-fifo");
        create_dir(&dir.0).unwrap();
        let fifo = dir.0.join("a.example.com");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path that outlives the call
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);

        // without a reader a blocking open would hang right here
        let err = open_for_write(&fifo).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENXIO));

        let mut files = ResolverFiles::new(&dir.0, "utun1");
        files.remove_stale(|_| false).unwrap();
        files
            .write("10.2.2.17", &domains(&["a.example.com"]))
            .unwrap();
        files.remove().unwrap();
        assert!(std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed() {
        use super::resolver::open_for_write;
        use std::os::unix::fs::symlink;

        let dir = TempDir::new("resolver-symlink");
        let base = dir.0.join("resolver");
        create_dir(&base).unwrap();
        let target = dir.0.join("target");
        std::fs::write(
            &target,
            "# managed by corplink-rs for interface \"utun1\"\n",
        )
        .unwrap();
        let link = base.join("intranet.example.com");
        symlink(&target, &link).unwrap();

        let mut files = ResolverFiles::new(&base, "utun1");
        files
            .write("10.2.2.17", &domains(&["intranet.example.com"]))
            .unwrap();
        files.remove_stale(|_| false).unwrap();
        files.remove().unwrap();
        assert_eq!(
            read(&target),
            "# managed by corplink-rs for interface \"utun1\"\n"
        );
        assert!(link.is_symlink());

        // the open itself refuses a symlink swapped in after the checks
        let err = open_for_write(&link).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP));
        assert_eq!(
            read(&target),
            "# managed by corplink-rs for interface \"utun1\"\n"
        );
    }
}

#[cfg(target_os = "linux")]
impl DNSManager {
    pub fn set_dns(&mut self, dns_servers: Vec<&str>, dns_search: Vec<&str>) -> Result<()> {
        if dns_servers.is_empty() {
            return Ok(());
        }

        if self.backup_path.exists() {
            log::warn!(
                "existing backup at {} — a previous instance likely did not exit \
                 gracefully; keeping that file as the authoritative pre-override",
                self.backup_path.display()
            );
        } else {
            if let Err(e) = fs::rename(RESOLV_CONF_PATH, &self.backup_path) {
                log::warn!(
                    "could not back up {} to {}: {e}. \
                     Overriding without backup; restore on exit will be a no-op.",
                    RESOLV_CONF_PATH,
                    self.backup_path.display()
                );
            } else {
                log::info!(
                    "renamed {} -> {} for backup",
                    RESOLV_CONF_PATH,
                    self.backup_path.display()
                );
            }
        }

        let new_content = render_resolv_conf(&dns_servers, &dns_search);
        fs::write(RESOLV_CONF_PATH, &new_content)
            .with_context(|| format!("failed to write {RESOLV_CONF_PATH}"))?;

        log::info!(
            "DNS overridden in {}; servers={:?} search={:?}",
            RESOLV_CONF_PATH,
            dns_servers,
            dns_search
        );
        Ok(())
    }

    pub fn restore_dns(&self) -> Result<()> {
        if !self.backup_path.exists() {
            return Ok(());
        }
        match fs::rename(&self.backup_path, RESOLV_CONF_PATH) {
            Ok(()) => {
                log::info!(
                    "restored {} from {} (via rename)",
                    RESOLV_CONF_PATH,
                    self.backup_path.display()
                );
                Ok(())
            }
            Err(e) => {
                log::warn!(
                    "could not restore {} by renaming {} back: {e}. \
                     Leaving backup on disk.",
                    RESOLV_CONF_PATH,
                    self.backup_path.display()
                );
                Ok(())
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl DNSManager {
    pub fn set_dns(&mut self, _dns_servers: Vec<&str>, _dns_search: Vec<&str>) -> Result<()> {
        Ok(())
    }
    pub fn restore_dns(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn render_resolv_conf(dns_servers: &[&str], dns_search: &[&str]) -> String {
    let mut out = String::new();
    out.push_str("# Generated by corplink-rs (will be restored on graceful exit)\n");
    for dns in dns_servers {
        out.push_str(&format!("nameserver {dns}\n"));
    }
    if !dns_search.is_empty() {
        out.push_str(&format!("search {}\n", dns_search.join(" ")));
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_filename_when_none_given() {
        let m = DNSManager::new(None);
        let expected = Path::new(RESOLV_CONF_PATH)
            .parent()
            .unwrap()
            .join(LINUX_DEFAULT_BACKUP_FILENAME);
        assert_eq!(m.backup_path(), expected.as_path());
    }

    #[test]
    fn default_filename_when_empty_string_given() {
        let m = DNSManager::new(Some(String::new()));
        let expected = Path::new(RESOLV_CONF_PATH)
            .parent()
            .unwrap()
            .join(LINUX_DEFAULT_BACKUP_FILENAME);
        assert_eq!(m.backup_path(), expected.as_path());
    }

    #[test]
    fn custom_filename_joined_with_resolv_conf_parent() {
        let m = DNSManager::new(Some("my.bak".to_string()));
        assert_eq!(m.backup_path(), Path::new("/etc/my.bak"));
    }

    #[test]
    fn backup_path_always_in_resolv_conf_dir() {
        // Invariant: because we only take a filename and join it with
        // RESOLV_CONF_PATH's parent, the backup is always on the same fs
        // as /etc/resolv.conf — rename(2) cannot EXDEV.
        let resolv_dir = Path::new(RESOLV_CONF_PATH).parent().unwrap();
        for filename in ["resolv.conf.corplink", "other.bak", "x"] {
            let m = DNSManager::new(Some(filename.to_string()));
            assert_eq!(m.backup_path().parent().unwrap(), resolv_dir);
        }
    }

    #[test]
    fn render_single_dns_no_search() {
        let out = render_resolv_conf(&["10.8.8.18"], &[]);
        assert!(out.contains("nameserver 10.8.8.18\n"));
        assert!(!out.contains("search "));
    }

    #[test]
    fn render_multiple_dns() {
        let out = render_resolv_conf(&["10.8.8.18", "114.114.114.114"], &[]);
        assert!(out.contains("nameserver 10.8.8.18\n"));
        assert!(out.contains("nameserver 114.114.114.114\n"));
    }

    #[test]
    fn render_with_search_domains() {
        let out = render_resolv_conf(&["10.8.8.18"], &["bytedance.net", "corp.local"]);
        assert!(out.contains("search bytedance.net corp.local\n"));
    }

    #[test]
    fn render_starts_with_comment_marker() {
        let out = render_resolv_conf(&["1.1.1.1"], &[]);
        assert!(out.starts_with("# "), "expected a comment banner, got: {out}");
    }
}
