//! The execution environment (`environment/ServerEnvironment.ts`, `ServerEnvironmentLabel.ts`,
//! `ServerEnvironmentMachine.ts`, `RemoteOpenTargets.ts`; plan §6.15): the environment id, the
//! label and machine kind of this host, the descriptor served at
//! `GET /.well-known/t3/environment` and inside `ServerConfig.environment`, and the SSH targets
//! advertised for remote open-in-editor links.
//!
//! Capabilities gate UI features, so the descriptor advertises only what this server
//! implements: each package sets its own flags in [`Capabilities`] when it plugs in (see
//! `app::plugins`).

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};
use zc_contracts::{ExecutionEnvironmentCapabilities, ExecutionEnvironmentDescriptor};
use zc_core::process::{run_process, ProcessRunInput, TimeoutBehavior};

/// `apps/server/package.json` `version`: the web client compares it with its own build and shows
/// a "server out of date" banner when the server is older.
pub const SERVER_VERSION: &str = "0.0.43";

/// `ORCHESTRATION_PROTOCOL_VERSION`.
pub const ORCHESTRATION_PROTOCOL_VERSION: i64 = 1;

/// The `ExecutionEnvironmentCapabilities` flags this server advertises, in any order (they are
/// encoded in declaration order). Absent flags read as unsupported on the client.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    flags: Map<String, Value>,
}

impl Capabilities {
    /// Advertise `key` (a boolean flag, or a value such as `fileAttachments`).
    pub fn set(&mut self, key: &str, value: impl Into<Value>) -> &mut Self {
        self.flags.insert(key.to_owned(), value.into());
        self
    }

    /// Advertise several boolean flags as `true`.
    pub fn enable(&mut self, keys: &[&str]) -> &mut Self {
        for key in keys {
            self.set(key, true);
        }
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.flags.get(key)
    }

    /// The typed capabilities (declaration order on the wire). `repositoryIdentity` is required
    /// and defaults to `false`.
    pub fn to_typed(&self) -> Result<ExecutionEnvironmentCapabilities, serde_json::Error> {
        let mut flags = self.flags.clone();
        flags.entry("repositoryIdentity").or_insert(Value::Bool(false));
        serde_json::from_value(Value::Object(flags))
    }
}

/// `platformOs`.
pub fn platform_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => "unknown",
    }
}

/// `platformArch`.
pub fn platform_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => "other",
    }
}

fn normalize(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)
}

/// Runs a probe command (5 s timeout): its trimmed stdout when it exits 0.
async fn probe(command: &str, args: &[&str]) -> Option<String> {
    let mut input = ProcessRunInput::new(command, args.iter().copied());
    input.timeout = Some(Duration::from_secs(5));
    input.timeout_behavior = TimeoutBehavior::TimedOutResult;
    match run_process(input).await {
        Ok(output) if output.code == Some(0) => normalize(Some(&output.stdout)),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(command, %error, "environment probe failed");
            None
        }
    }
}

/// `parseMachineInfoValue` (`/etc/machine-info`).
fn parse_machine_info_value(raw: &str, key: &str) -> Option<String> {
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(value) = trimmed.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')) else {
            continue;
        };
        let value = value.trim();
        let unquoted = if value.len() >= 2 && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\''))) {
            &value[1..value.len() - 1]
        } else {
            value
        };
        return normalize(Some(unquoted));
    }
    None
}

/// The host name (`os.hostname()`).
#[cfg(unix)]
pub fn hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and length describe `buffer`, which outlives the call.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    normalize(Some(&String::from_utf8_lossy(&buffer[..end])))
}

/// The host name (`os.hostname()`).
#[cfg(not(unix))]
pub fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().and_then(|v| normalize(Some(&v)))
}

/// `resolveServerEnvironmentLabel`: the macOS computer name, the Linux pretty host name, the
/// host name, the cwd's base name, then `T3 environment`.
pub async fn resolve_label(cwd: &Path) -> String {
    let friendly = match std::env::consts::OS {
        "macos" => probe("scutil", &["--get", "ComputerName"]).await,
        "linux" => {
            let machine_info = tokio::fs::read_to_string("/etc/machine-info").await.ok();
            match machine_info.as_deref().and_then(|raw| parse_machine_info_value(raw, "PRETTY_HOSTNAME")) {
                Some(pretty) => Some(pretty),
                None => probe("hostnamectl", &["--pretty"]).await,
            }
        }
        _ => None,
    };
    friendly
        .or_else(hostname)
        .or_else(|| normalize(cwd.file_name().map(|name| name.to_string_lossy()).as_deref()))
        .unwrap_or_else(|| "T3 environment".to_owned())
}

/// `machineKindFromAppleProductName`.
pub fn machine_kind_from_apple_product_name(name: &str) -> Option<&'static str> {
    let normalized: String = name.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if normalized.starts_with("macmini") {
        Some("mac-mini")
    } else if normalized.starts_with("macstudio") {
        Some("mac-studio")
    } else if normalized.starts_with("macbook") {
        Some("laptop")
    } else if normalized.starts_with("imac") || normalized.starts_with("macpro") {
        Some("desktop")
    } else {
        None
    }
}

const VIRTUALIZATION_MARKERS: &[&str] = &[
    "qemu",
    "kvm",
    "bochs",
    "vmware",
    "virtualbox",
    "innotek",
    "xen",
    "parallels",
    "amazon ec2",
    "google compute engine",
    "digitalocean",
    "hetzner",
    "linode",
    "vultr",
    "scaleway",
    "openstack",
    "cloud",
    "virtual machine",
];

/// `machineKindFromDmi`.
pub fn machine_kind_from_dmi(chassis_type: Option<&str>, sys_vendor: Option<&str>, product_name: Option<&str>) -> Option<&'static str> {
    let product = product_name.unwrap_or("");
    let vendor_and_product = format!("{} {}", sys_vendor.unwrap_or(""), product).to_lowercase();
    if VIRTUALIZATION_MARKERS.iter().any(|marker| vendor_and_product.contains(marker)) {
        return Some("cloud");
    }
    if let Some(kind) = machine_kind_from_apple_product_name(product) {
        return Some(kind);
    }
    match chassis_type? {
        "3" | "4" | "5" | "6" | "7" | "13" | "15" | "16" | "35" => Some("desktop"),
        "8" | "9" | "10" | "14" | "31" | "32" => Some("laptop"),
        "17" | "18" | "19" | "20" | "21" | "22" | "23" | "24" | "28" => Some("server"),
        _ => None,
    }
}

async fn read_optional(path: &str) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok().and_then(|v| normalize(Some(&v)))
}

/// `detectServerEnvironmentMachineKind`: best effort, `None` means "no signal".
pub async fn detect_machine_kind() -> Option<&'static str> {
    match std::env::consts::OS {
        "macos" => {
            let ioreg = probe("ioreg", &["-rd1", "-n", "product"]).await;
            let product_name = ioreg.as_deref().and_then(|raw| {
                let start = raw.find("\"product-name\"")?;
                let rest = &raw[start..];
                let open = rest.find("<\"")? + 2;
                let close = rest[open..].find("\">")?;
                Some(rest[open..open + close].to_owned())
            });
            if let Some(kind) = product_name.as_deref().and_then(machine_kind_from_apple_product_name) {
                return Some(kind);
            }
            let model = probe("sysctl", &["-n", "hw.model"]).await?;
            machine_kind_from_apple_product_name(&model)
        }
        "linux" => {
            let kernel = read_optional("/proc/sys/kernel/osrelease").await;
            if kernel.is_some_and(|k| k.to_lowercase().contains("microsoft")) {
                return Some("linux");
            }
            let chassis = read_optional("/sys/class/dmi/id/chassis_type").await;
            let vendor = read_optional("/sys/class/dmi/id/sys_vendor").await;
            let product = read_optional("/sys/class/dmi/id/product_name").await;
            machine_kind_from_dmi(chassis.as_deref(), vendor.as_deref(), product.as_deref())
        }
        _ => None,
    }
}

/// `ServerEnvironment`: the environment id and the descriptor, computed once at startup.
#[derive(Clone, Debug)]
pub struct ServerEnvironment {
    environment_id: String,
    descriptor: ExecutionEnvironmentDescriptor,
    descriptor_json: Value,
}

impl ServerEnvironment {
    /// Builds the descriptor. `agentActivityPublishing` is always `false`: T3 Connect is inert in
    /// zenith (plan §6.14).
    pub fn new(environment_id: &str, label: &str, machine: Option<&str>, capabilities: &Capabilities) -> anyhow::Result<Self> {
        let mut capabilities = capabilities.clone();
        capabilities.set("agentActivityPublishing", false);
        let mut platform = json!({ "os": platform_os(), "arch": platform_arch() });
        if let Some(machine) = machine {
            platform["machine"] = json!(machine);
        }
        let descriptor: ExecutionEnvironmentDescriptor = serde_json::from_value(json!({
            "environmentId": environment_id,
            "label": label,
            "platform": platform,
            "serverVersion": SERVER_VERSION,
            "orchestrationProtocolVersion": ORCHESTRATION_PROTOCOL_VERSION,
            "capabilities": serde_json::to_value(capabilities.to_typed()?)?,
        }))?;
        let descriptor_json = serde_json::to_value(&descriptor)?;
        Ok(Self {
            environment_id: environment_id.to_owned(),
            descriptor,
            descriptor_json,
        })
    }

    /// Probes the label and machine kind of this host, then builds the descriptor.
    pub async fn detect(environment_id: &str, cwd: &Path, capabilities: &Capabilities) -> anyhow::Result<Self> {
        let (label, machine) = tokio::join!(resolve_label(cwd), detect_machine_kind());
        Self::new(environment_id, &label, machine, capabilities)
    }

    pub fn environment_id(&self) -> &str {
        &self.environment_id
    }

    pub fn descriptor(&self) -> &ExecutionEnvironmentDescriptor {
        &self.descriptor
    }

    /// The encoded descriptor.
    pub fn descriptor_json(&self) -> &Value {
        &self.descriptor_json
    }
}

/// Whether something accepts TCP connections on `host:port` within 250 ms
/// (`NetService.hasListenerOnHost`).
async fn has_listener(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(250), tokio::net::TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

/// `normalizeMagicDnsName` over `tailscale status --json`.
pub fn parse_magic_dns_name(raw: &str) -> Option<String> {
    let status: Value = serde_json::from_str(raw).ok()?;
    let name = status.get("Self")?.get("DNSName")?.as_str()?;
    normalize(Some(name.trim().trim_end_matches('.')))
}

/// `RemoteOpenTargets.resolveTargets`: nothing unless sshd listens on loopback; then the
/// tailnet name (when tailscale is up) and `<short hostname>.local`, most reachable first.
pub async fn resolve_remote_open_targets() -> Vec<Value> {
    let (ipv4, ipv6) = tokio::join!(has_listener("127.0.0.1", 22), has_listener("::1", 22));
    if !ipv4 && !ipv6 {
        return Vec::new();
    }
    let mut targets = Vec::new();
    if let Some(name) = probe("tailscale", &["status", "--json"]).await.as_deref().and_then(parse_magic_dns_name) {
        targets.push(json!({ "kind": "tailscale", "host": name }));
    }
    if let Some(short) = hostname().as_deref().and_then(|h| h.split('.').next()).map(str::trim).filter(|s| !s.is_empty()) {
        targets.push(json!({ "kind": "mdns", "host": format!("{short}.local") }));
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_kinds() {
        assert_eq!(machine_kind_from_apple_product_name("Mac mini (2024)"), Some("mac-mini"));
        assert_eq!(machine_kind_from_apple_product_name("Macmini9,1"), Some("mac-mini"));
        assert_eq!(machine_kind_from_apple_product_name("MacBookPro18,3"), Some("laptop"));
        assert_eq!(machine_kind_from_apple_product_name("Mac Studio"), Some("mac-studio"));
        assert_eq!(machine_kind_from_apple_product_name("iMac21,1"), Some("desktop"));
        assert_eq!(machine_kind_from_apple_product_name("Raspberry"), None);
        assert_eq!(machine_kind_from_dmi(Some("10"), Some("LENOVO"), Some("ThinkPad")), Some("laptop"));
        assert_eq!(machine_kind_from_dmi(Some("3"), Some("QEMU"), Some("Standard PC")), Some("cloud"));
        assert_eq!(machine_kind_from_dmi(Some("23"), None, None), Some("server"));
        assert_eq!(machine_kind_from_dmi(Some("2"), None, None), None);
    }

    #[test]
    fn machine_info() {
        let raw = "# comment\nPRETTY_HOSTNAME=\"Studio box\"\nCHASSIS=desktop\n";
        assert_eq!(parse_machine_info_value(raw, "PRETTY_HOSTNAME").as_deref(), Some("Studio box"));
        assert_eq!(parse_machine_info_value(raw, "CHASSIS").as_deref(), Some("desktop"));
        assert_eq!(parse_machine_info_value(raw, "MISSING"), None);
    }

    #[test]
    fn magic_dns() {
        assert_eq!(
            parse_magic_dns_name(r#"{"Self":{"DNSName":"box.tail.ts.net."}}"#).as_deref(),
            Some("box.tail.ts.net")
        );
        assert_eq!(parse_magic_dns_name(r#"{"Self":{}}"#), None);
        assert_eq!(parse_magic_dns_name("nope"), None);
    }

    #[test]
    fn descriptor_encodes_in_declaration_order() {
        let mut capabilities = Capabilities::default();
        capabilities.enable(&["environmentIcon", "connectionProbe"]);
        let environment = ServerEnvironment::new("environment-1", "Test Machine", Some("mac-mini"), &capabilities).unwrap();
        let encoded = serde_json::to_string(environment.descriptor_json()).unwrap();
        assert_eq!(
            encoded,
            r#"{"environmentId":"environment-1","label":"Test Machine","platform":{"os":"#.to_owned()
                + &format!("\"{}\",\"arch\":\"{}\",\"machine\":\"mac-mini\"}},", platform_os(), platform_arch())
                + r#""serverVersion":"0.0.43","orchestrationProtocolVersion":1,"capabilities":{"repositoryIdentity":false,"connectionProbe":true,"agentActivityPublishing":false,"environmentIcon":true}}"#
        );
    }
}
