use std::fs::{self, File};
use std::io::Write;
#[cfg(feature = "miyoo")]
use tokio::process::Command;

use anyhow::Result;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

use crate::constants::ALLIUM_WIFI_SETTINGS;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WiFiSettings {
    pub wifi: bool,
    pub ssid: String,
    pub password: String,
    pub ntp: bool,
    pub web_file_browser: bool,
    pub telnet: bool,
    #[serde(default)]
    pub ssh: bool,
    pub ftp: bool,
    pub syncthing: bool,
    pub scraper: bool,
}

impl WiFiSettings {
    pub fn new() -> Self {
        Self {
            wifi: false,
            ssid: String::new(),
            password: String::new(),
            ntp: false,
            web_file_browser: false,
            telnet: false,
            ssh: false,
            ftp: false,
            syncthing: false,
            scraper: false,
        }
    }

    pub fn load() -> Result<Self> {
        if ALLIUM_WIFI_SETTINGS.exists() {
            debug!("found state, loading from file");
            if let Ok(json) = fs::read_to_string(ALLIUM_WIFI_SETTINGS.as_path())
                && let Ok(json) = serde_json::from_str(&json)
            {
                return Ok(json);
            }
            warn!("failed to read state file, removing");
            fs::remove_file(ALLIUM_WIFI_SETTINGS.as_path())?;
        }
        Ok(Self::load_wpa_supplicant_conf().unwrap_or_default())
    }

    pub fn init(&self) -> Result<()> {
        if self.wifi {
            wifi_on()?;
            telnet_off()?;
            ssh_off()?;
            if self.ntp {
                info!("Starting NTP...");
                ntp_sync()?;
            }
            if self.telnet {
                info!("Starting Telnet...");
                telnet_on()?;
            }
            if self.ssh {
                info!("Starting SSH...");
                ssh_on()?;
            }
            if self.ftp {
                info!("Starting FTP...");
                ftp_on()?;
            }
            if self.web_file_browser {
                info!("Starting Web File Browser...");
                web_file_browser_on()?;
            }
            if self.syncthing {
                info!("Starting Syncthing...");
                syncthing_on()?;
            }
            if self.scraper {
                info!("Starting Box Art Scraper...");
                scraper_on()?;
            }
        }
        Ok(())
    }

    #[cfg(not(feature = "miyoo"))]
    pub async fn init_sequenced(&self) -> Result<()> {
        self.init()
    }

    #[cfg(feature = "miyoo")]
    pub async fn init_sequenced(&self) -> Result<()> {
        if self.wifi {
            wifi_on()?;
            telnet_off()?;
            sequence_ssh(
                || run_ssh_script("ssh-off.sh"),
                || run_ssh_script("ssh-on.sh"),
                self.ssh,
            )
            .await?;
            if self.ntp {
                info!("Starting NTP...");
                ntp_sync()?;
            }
            if self.telnet {
                info!("Starting Telnet...");
                telnet_on()?;
            }
            if self.ftp {
                info!("Starting FTP...");
                ftp_on()?;
            }
            if self.web_file_browser {
                info!("Starting Web File Browser...");
                web_file_browser_on()?;
            }
            if self.syncthing {
                info!("Starting Syncthing...");
                syncthing_on()?;
            }
            if self.scraper {
                info!("Starting Box Art Scraper...");
                scraper_on()?;
            }
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string(&self).unwrap();
        File::create(ALLIUM_WIFI_SETTINGS.as_path())?.write_all(json.as_bytes())?;
        if let Err(e) = self.update_wpa_supplicant_conf() {
            warn!("failed to update wpa_supplicant.conf: {}", e);
        }
        Ok(())
    }

    fn load_wpa_supplicant_conf() -> Option<Self> {
        #[cfg(feature = "miyoo")]
        {
            let data = fs::read_to_string("/appconfigs/wpa_supplicant.conf").ok()?;

            let ssid_index = data.find("ssid=\"")?;
            let ssid = &data[ssid_index + 6..];
            let ssid_end = ssid.find('"')?;
            let ssid = &ssid[..ssid_end];

            let psk_index = data.find("psk=\"")?;
            let psk = &data[psk_index + 5..];
            let psk_end = psk.find('"')?;
            let psk = &psk[..psk_end];

            Some(Self {
                ssid: ssid.to_string(),
                password: psk.to_string(),
                ..Default::default()
            })
        }

        #[cfg(not(feature = "miyoo"))]
        Some(Self::new())
    }

    fn update_wpa_supplicant_conf(&self) -> Result<()> {
        #[cfg(feature = "miyoo")]
        {
            let mut file = File::create("/appconfigs/wpa_supplicant.conf")?;
            write!(
                file,
                r#"ctrl_interface=/var/run/wpa_supplicant
update_config=1

network={{
	ssid="{ssid}"
	psk="{password}"
}}"#,
                ssid = self.ssid.replace('"', "\\\""),
                password = self.password.replace('"', "\\\""),
            )?;
        }
        Ok(())
    }

    pub fn set_wifi(&mut self, enabled: bool) -> Result<()> {
        self.wifi = enabled;
        if self.wifi {
            wifi_on()?;
            let telnet = self.telnet;
            let ssh = self.ssh;
            let ftp = self.ftp;
            tokio::spawn(async move {
                if wait_for_wifi().await.is_ok() {
                    if telnet {
                        telnet_on().ok();
                    }
                    if ssh {
                        ssh_on().ok();
                    }
                    if ftp {
                        ftp_on().ok();
                    }
                }
            });
        } else {
            wifi_off()?;
            if self.telnet {
                telnet_off().ok();
            }
            if self.ssh {
                ssh_off().ok();
            }
            if self.ftp {
                ftp_off().ok();
            }
        }
        Ok(())
    }

    pub fn set_ssid(&mut self, ssid: String) -> Result<()> {
        self.ssid = ssid;
        if self.wifi {
            self.set_wifi(self.wifi)?;
        }
        Ok(())
    }

    pub fn set_password(&mut self, password: String) -> Result<()> {
        self.password = password;
        if self.wifi {
            self.set_wifi(self.wifi)?;
        }
        Ok(())
    }

    pub fn toggle_ntp(&mut self, enabled: bool) -> Result<()> {
        self.ntp = enabled;
        if self.ntp {
            ntp_sync()?;
        }
        Ok(())
    }

    pub fn toggle_web_file_browser(&mut self, enabled: bool) -> Result<()> {
        self.web_file_browser = enabled;
        if self.web_file_browser {
            web_file_browser_on()?;
        } else {
            web_file_browser_off()?;
        }
        Ok(())
    }

    pub fn toggle_telnet(&mut self, enabled: bool) -> Result<()> {
        self.telnet = enabled;
        if self.telnet {
            telnet_on()?;
        } else {
            telnet_off()?;
        }
        Ok(())
    }

    pub fn toggle_ssh(&mut self, enabled: bool) -> Result<()> {
        self.ssh = enabled;
        if self.ssh {
            ssh_on()?;
        } else {
            ssh_off()?;
        }
        Ok(())
    }

    pub fn toggle_ftp(&mut self, enabled: bool) -> Result<()> {
        self.ftp = enabled;
        if self.ftp {
            ftp_on()?;
        } else {
            ftp_off()?;
        }
        Ok(())
    }

    pub fn toggle_scraper(&mut self, enabled: bool) -> Result<()> {
        self.scraper = enabled;
        if self.scraper {
            scraper_on()?;
        } else {
            scraper_off()?;
        }
        Ok(())
    }

    pub fn toggle_syncthing(&mut self, enabled: bool) -> Result<()> {
        self.syncthing = enabled;
        if self.syncthing {
            syncthing_on()?;
        } else {
            syncthing_off()?;
        }
        Ok(())
    }
}

impl Default for WiFiSettings {
    fn default() -> Self {
        Self::new()
    }
}

pub fn wifi_on() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("wifi-on.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn wifi-on.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("wifi-on.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn wifi_off() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("wifi-off.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn wifi-off.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("wifi-off.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn telnet_on() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("telnet-on.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn telnet-on.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("telnet-on.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn telnet_off() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("telnet-off.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn telnet-off.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("telnet-off.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn ssh_on() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("ssh-on.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn ssh-on.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("ssh-on.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn ssh_off() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("ssh-off.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn ssh-off.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("ssh-off.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn ftp_on() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("ftp-on.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn ftp-on.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("ftp-on.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn ftp_off() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("ftp-off.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn ftp-off.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("ftp-off.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn ntp_sync() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("ntp-sync.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn ntp-sync.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("ntp-sync.sh failed: {}", e);
                e
            })
            .ok();

        // Reset start time if time changed
        match crate::game_info::GameInfo::load() {
            Ok(Some(mut game_info)) => {
                game_info.start_time = chrono::Utc::now();
                game_info
                    .save()
                    .map_err(|e| {
                        log::error!("failed to save game info: {}", e);
                        e
                    })
                    .ok();
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("failed to load game info: {}", e);
            }
        }
    });
    Ok(())
}

pub fn web_file_browser_on() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("dufs-on.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn dufs-on.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("dufs-on.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn web_file_browser_off() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("dufs-off.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn dufs-off.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("dufs-off.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn scraper_on() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("collie-on.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn collie-on.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("collie-on.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn scraper_off() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("collie-off.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn collie-off.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("collie-off.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn syncthing_on() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("syncthing-on.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn syncthing-on.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("syncthing-on.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub fn syncthing_off() -> Result<()> {
    #[cfg(feature = "miyoo")]
    tokio::spawn(async {
        Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("syncthing-off.sh"))
            .spawn()
            .map_err(|e| {
                log::error!("failed to spawn syncthing-off.sh: {}", e);
                e
            })
            .unwrap()
            .wait()
            .await
            .map_err(|e| {
                log::error!("syncthing-off.sh failed: {}", e);
                e
            })
    });
    Ok(())
}

pub async fn wait_for_wifi() -> Result<()> {
    #[cfg(feature = "miyoo")]
    Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join("wait-for-wifi.sh"))
        .spawn()
        .map_err(|e| {
            log::error!("failed to spawn wait-for-wifi.sh: {}", e);
            e
        })
        .ok()
        .unwrap()
        .wait()
        .await
        .ok();
    Ok(())
}

pub fn ip_address() -> Option<String> {
    sample_ip_address()
        .ok()
        .flatten()
        .map(|address| address.to_string())
}

pub(crate) fn sample_ip_address() -> Result<Option<std::net::Ipv4Addr>> {
    #[cfg(feature = "miyoo")]
    {
        // An interface address alone does not establish which route owns the
        // limited broadcast destination used by the compatibility probe.
        if wlan0_has_broadcast_route().unwrap_or(false) {
            if let Ok(address) = interface_ip_address() {
                return Ok(address);
            }
        }
        let child = std::process::Command::new("ip")
            .args(["route", "get", "255.255.255.255"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        decode_route(bounded_output(
            child,
            std::time::Duration::from_millis(300),
        )?)
    }
    #[cfg(all(feature = "simulator", not(feature = "miyoo")))]
    {
        Ok(Some(std::net::Ipv4Addr::LOCALHOST))
    }
    #[cfg(not(any(feature = "miyoo", feature = "simulator")))]
    {
        Ok(None)
    }
}

#[cfg(feature = "miyoo")]
fn wlan0_has_broadcast_route() -> std::io::Result<bool> {
    use std::io::Read;
    const LIMIT: usize = 8192;
    let mut bytes = Vec::with_capacity(1024);
    std::fs::File::open("/proc/net/route")?
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes.len() <= LIMIT && std::str::from_utf8(&bytes).is_ok_and(proc_route_uses_wlan0))
}

#[cfg(any(feature = "miyoo", test))]
fn proc_route_uses_wlan0(table: &str) -> bool {
    fn parse(table: &str) -> Option<bool> {
        let mut lines = table.lines();
        if !lines.next()?.split_whitespace().eq([
            "Iface",
            "Destination",
            "Gateway",
            "Flags",
            "RefCnt",
            "Use",
            "Metric",
            "Mask",
            "MTU",
            "Window",
            "IRTT",
        ]) {
            return None;
        }
        let mut best = None;
        for line in lines.filter(|line| !line.trim().is_empty()) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 11 {
                return None;
            }
            // /proc/net/route prints IPv4 words in host byte order.
            let destination =
                u32::from_be_bytes(u32::from_str_radix(fields[1], 16).ok()?.to_ne_bytes());
            u32::from_str_radix(fields[2], 16).ok()?;
            let flags = u32::from_str_radix(fields[3], 16).ok()?;
            let metric = fields[6].parse::<u32>().ok()?;
            for index in [4, 5, 8, 9, 10] {
                fields[index].parse::<u32>().ok()?;
            }
            let mask = u32::from_be_bytes(u32::from_str_radix(fields[7], 16).ok()?.to_ne_bytes());
            let prefix = mask.leading_ones();
            if mask != u32::MAX.checked_shl(32 - prefix).unwrap_or(0) {
                return None;
            }
            if flags & 0x1 == 0 || destination & mask != mask {
                continue;
            }
            // Reject entries and equal-cost routes on another interface are
            // deliberately ambiguous: let ip resolve them rather than guess.
            let usable = fields[0] == "wlan0" && flags & 0x200 == 0;
            match best {
                Some((length, cost, _))
                    if prefix < length || (prefix == length && metric > cost) => {}
                Some((length, cost, previous)) if prefix == length && metric == cost => {
                    best = Some((prefix, metric, previous && usable));
                }
                _ => best = Some((prefix, metric, usable)),
            }
        }
        Some(best.is_some_and(|(_, _, usable)| usable))
    }
    parse(table).unwrap_or(false)
}

#[cfg(all(feature = "miyoo", target_os = "linux"))]
fn interface_ip_address() -> std::io::Result<Option<std::net::Ipv4Addr>> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // OwnedFd closes the datagram socket on every return path. No traffic is sent.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // ifreq is a C POD. The kernel fills its union for each requested ioctl.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dest, source) in request.ifr_name.iter_mut().zip(b"wlan0") {
        *dest = *source as libc::c_char;
    }
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) } < 0 {
        return missing_interface_or_error();
    }
    if unsafe { request.ifr_ifru.ifru_flags } as i32 & libc::IFF_UP == 0 {
        return Ok(None);
    }
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFADDR, &mut request) } < 0 {
        return missing_interface_or_error();
    }
    // SIOCGIFADDR returns an AF_INET sockaddr in the aligned ifreq union.
    let address = unsafe {
        &*(&request.ifr_ifru.ifru_addr as *const libc::sockaddr).cast::<libc::sockaddr_in>()
    };
    Ok(decode_interface_address(address.sin_addr.s_addr))
}

#[cfg(all(feature = "miyoo", target_os = "linux"))]
fn missing_interface_or_error() -> std::io::Result<Option<std::net::Ipv4Addr>> {
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENODEV | libc::ENXIO | libc::EADDRNOTAVAIL) => Ok(None),
        _ => Err(error),
    }
}

#[cfg(all(feature = "miyoo", not(target_os = "linux")))]
fn interface_ip_address() -> std::io::Result<Option<std::net::Ipv4Addr>> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(any(all(feature = "miyoo", target_os = "linux"), test))]
fn decode_interface_address(network_order: u32) -> Option<std::net::Ipv4Addr> {
    let address = std::net::Ipv4Addr::from(network_order.to_ne_bytes());
    (!address.is_unspecified()).then_some(address)
}

#[cfg(any(feature = "miyoo", test))]
pub(crate) fn bounded_output(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> anyhow::Result<Vec<u8>> {
    let result = collect_probe_output(&mut child, std::time::Instant::now() + timeout);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let output = result?;
    anyhow::ensure!(status.success(), "route probe failed with {status}");
    Ok(output)
}

#[cfg(any(feature = "miyoo", test))]
fn collect_probe_output(
    child: &mut std::process::Child,
    deadline: std::time::Instant,
) -> anyhow::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing probe stdout"))?;
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut output = Vec::with_capacity(512);
    loop {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "route probe timed out"
        );
        drain_probe_stdout(&mut stdout, &mut output)?;
        if child.try_wait()?.is_some() {
            drain_probe_stdout(&mut stdout, &mut output)?;
            return Ok(output);
        }
        std::thread::sleep(
            std::time::Duration::from_millis(10)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
}

#[cfg(any(feature = "miyoo", test))]
fn drain_probe_stdout(
    stdout: &mut std::process::ChildStdout,
    output: &mut Vec<u8>,
) -> anyhow::Result<()> {
    use std::io::{ErrorKind, Read};
    let mut buffer = [0; 512];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                anyhow::ensure!(output.len() + count <= 4096, "route probe output too large");
                output.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(any(feature = "miyoo", test))]
pub(crate) fn decode_route(output: Vec<u8>) -> anyhow::Result<Option<std::net::Ipv4Addr>> {
    let output = String::from_utf8(output)?;
    let mut words = output.split_whitespace();
    while let Some(word) = words.next() {
        if word == "src" {
            let address = words
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing source address"))?;
            return Ok(Some(address.parse()?));
        }
    }
    Ok(None)
}

#[cfg(feature = "miyoo")]
async fn run_ssh_script(name: &str) -> Result<()> {
    use anyhow::{Context, ensure};
    let status = Command::new(crate::constants::ALLIUM_SCRIPTS_DIR.join(name))
        .status()
        .await
        .with_context(|| format!("starting {name}"))?;
    ensure!(status.success(), "{name} failed with {status}");
    Ok(())
}

#[cfg(any(feature = "miyoo", test))]
async fn sequence_ssh<Off, On, OffFuture, OnFuture>(off: Off, on: On, enabled: bool) -> Result<()>
where
    Off: FnOnce() -> OffFuture,
    On: FnOnce() -> OnFuture,
    OffFuture: std::future::Future<Output = Result<()>>,
    OnFuture: std::future::Future<Output = Result<()>>,
{
    use anyhow::Context;
    off().await.context("stopping SSH before startup")?;
    if enabled {
        on().await.context("starting SSH after stop completed")?;
    }
    Ok(())
}

#[cfg(test)]
mod ssh_sequence_tests {
    use super::sequence_ssh;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn delayed_stop_completes_before_start_is_constructed() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let stopping = events.clone();
        let starting = events.clone();
        sequence_ssh(
            move || async move {
                stopping.lock().unwrap().push("off-start");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                stopping.lock().unwrap().push("off-complete");
                Ok(())
            },
            move || {
                starting.lock().unwrap().push("on-constructed");
                async move {
                    starting.lock().unwrap().push("on-complete");
                    Ok(())
                }
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            ["off-start", "off-complete", "on-constructed", "on-complete"]
        );
    }

    #[tokio::test]
    async fn stop_failure_never_constructs_start() {
        let error = sequence_ssh(
            || async { anyhow::bail!("stop fixture failed") },
            || {
                panic!("start must not be constructed after failed stop");
                #[allow(unreachable_code)]
                async {
                    Ok(())
                }
            },
            true,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("stopping SSH before startup: stop fixture failed"));
    }

    #[tokio::test]
    async fn start_failure_is_surfaced_and_disabled_setting_never_starts() {
        let error = sequence_ssh(
            || async { Ok(()) },
            || async { anyhow::bail!("start fixture failed") },
            true,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("starting SSH after stop completed: start fixture failed")
        );
        sequence_ssh(
            || async { Ok(()) },
            || {
                panic!("disabled SSH must not start");
                #[allow(unreachable_code)]
                async {
                    Ok(())
                }
            },
            false,
        )
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod status_address_tests {
    use super::*;

    const HEADER: &str = "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\n";

    #[test]
    fn proc_route_wlan0_default_accepts_ioctl() {
        assert!(proc_route_uses_wlan0(&format!(
            "{HEADER}wlan0 00000000 0101A8C0 0003 0 0 0 00000000 0 0 0\n"
        )));
    }

    #[test]
    fn proc_route_eth0_default_falls_back() {
        assert!(!proc_route_uses_wlan0(&format!(
            "{HEADER}eth0 00000000 0101A8C0 0003 0 0 0 00000000 0 0 0\n"
        )));
    }

    #[test]
    fn proc_route_no_broadcast_route_falls_back() {
        for table in [
            HEADER.to_owned(),
            format!("{HEADER}wlan0 0001A8C0 00000000 0001 0 0 0 00FFFFFF 0 0 0\n"),
        ] {
            assert!(!proc_route_uses_wlan0(&table));
        }
    }

    #[test]
    fn proc_route_reject_and_down_routes_fall_back() {
        for flags in ["0201", "0000"] {
            assert!(!proc_route_uses_wlan0(&format!(
                "{HEADER}wlan0 00000000 00000000 {flags} 0 0 0 00000000 0 0 0\n"
            )));
        }
    }

    #[test]
    fn proc_route_longest_prefix_and_metric_choose_interface() {
        let default = "wlan0 00000000 0101A8C0 0003 0 0 10 00000000 0 0 0\n";
        let broadcast = "wlan0 FFFFFFFF 00000000 0001 0 0 0 FFFFFFFF 0 0 0\n";
        assert!(proc_route_uses_wlan0(&format!(
            "{HEADER}{default}{broadcast}"
        )));
        for specific in [
            broadcast.replace("wlan0", "eth0"),
            broadcast.replace("0001", "0201"),
        ] {
            for table in [
                format!("{HEADER}{default}{specific}"),
                format!("{HEADER}{specific}{default}"),
            ] {
                assert!(!proc_route_uses_wlan0(&table));
            }
        }
        for metric in [0, 10] {
            assert!(!proc_route_uses_wlan0(&format!(
                "{HEADER}{default}eth0 00000000 0101A8C0 0003 0 0 {metric} 00000000 0 0 0\n"
            )));
        }
        assert!(proc_route_uses_wlan0(&format!(
            "{HEADER}{default}eth0 00000000 0101A8C0 0003 0 0 20 00000000 0 0 0\n"
        )));
    }

    #[test]
    fn proc_route_malformed_lines_fall_back_without_panicking() {
        for malformed in [
            "",
            "wlan0",
            "wlan0 zzz 0 1 0 0 0 00000000 0 0 0",
            "wlan0 00000000 00000000 nope 0 0 0 00000000 0 0 0",
            "wlan0 00000000 00000000 0001 0 0 nope 00000000 0 0 0",
            "wlan0 00000000 00000000 0001 0 0 0 00FF00FF 0 0 0",
            "wlan0 00000000 00000000 0001 bad 0 0 00000000 0 0 0",
        ] {
            assert!(!proc_route_uses_wlan0(malformed));
            assert!(!proc_route_uses_wlan0(&format!("{HEADER}{malformed}")));
        }
        assert!(!proc_route_uses_wlan0(&format!(
            "{HEADER}wlan0 00000000 00000000 0001 0 0 0 00000000 0 0 0\nbroken"
        )));
    }

    #[test]
    fn status_ioctl_address_preserves_network_byte_order_and_unspecified_is_offline() {
        assert_eq!(
            decode_interface_address(u32::from_ne_bytes([192, 168, 1, 23])),
            Some("192.168.1.23".parse().unwrap())
        );
        assert_eq!(decode_interface_address(0), None);
    }
}
