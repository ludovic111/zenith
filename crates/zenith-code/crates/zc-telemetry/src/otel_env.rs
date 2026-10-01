//! `packages/shared/src/otelEnvironment.ts`: the OpenTelemetry kill switch, exporter and
//! endpoint variables, and `resolveSignalEndpoint` (which collector each signal goes to).
//!
//! Order per signal: `T3CODE_OTLP_<SIGNAL>_URL` (with `T3CODE_OTLP_HEADERS`), then an
//! `OTEL_EXPORTER_OTLP_[<SIGNAL>_]ENDPOINT` with its own headers, then the settings.json value.
//! `T3CODE_OTEL_SDK_DISABLED` / `OTEL_SDK_DISABLED=true` turn every export off, and
//! `OTEL_<SIGNAL>_EXPORTER=none` one signal. Nothing is exported when nothing is configured.

use std::collections::{BTreeMap, BTreeSet};

use crate::trace::otlp::parse_otlp_headers;

/// What the OTEL variables say about one signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtelSignal {
    Unset,
    Off,
    Export {
        url: String,
        protocol: String,
        headers: Option<BTreeMap<String, String>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtelEnvironment {
    pub disabled: bool,
    pub warnings: Vec<String>,
    pub resource_attributes: BTreeMap<String, String>,
    pub traces: OtelSignal,
    pub metrics: OtelSignal,
    pub logs: OtelSignal,
}

impl OtelEnvironment {
    /// An environment that asked for nothing.
    pub fn none() -> Self {
        Self {
            disabled: false,
            warnings: Vec::new(),
            resource_attributes: BTreeMap::new(),
            traces: OtelSignal::Unset,
            metrics: OtelSignal::Unset,
            logs: OtelSignal::Unset,
        }
    }
}

fn blank_as_unset(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Setting<A> {
    value: Option<A>,
    warning: Option<String>,
}

impl<A> Setting<A> {
    fn claimed(&self) -> bool {
        self.value.is_some() || self.warning.is_some()
    }
}

const T3CODE_TRUE: &[&str] = &["true", "yes", "on", "1", "y"];
const T3CODE_FALSE: &[&str] = &["false", "no", "off", "0", "n"];
const NOT_EXPORTED: &str = "so the signals it configures are not exported";

fn flag(raw: Option<String>, truthy: &[&str], falsy: &[&str], invalid: impl Fn(&str) -> String) -> Setting<bool> {
    let Some(raw) = raw else {
        return Setting { value: None, warning: None };
    };
    let normalized = raw.trim().to_lowercase();
    if truthy.contains(&normalized.as_str()) {
        Setting {
            value: Some(true),
            warning: None,
        }
    } else if falsy.contains(&normalized.as_str()) {
        Setting {
            value: Some(false),
            warning: None,
        }
    } else if raw.trim().is_empty() {
        Setting { value: None, warning: None }
    } else {
        Setting {
            value: None,
            warning: Some(invalid(raw.trim())),
        }
    }
}

fn read_or_warn<A>(raw: Option<String>, parse: impl Fn(&str) -> Option<A>, warning: String) -> Setting<A> {
    match blank_as_unset(raw) {
        None => Setting { value: None, warning: None },
        Some(raw) => match parse(&raw) {
            Some(value) => Setting {
                value: Some(value),
                warning: None,
            },
            None => Setting {
                value: None,
                warning: Some(warning),
            },
        },
    }
}

fn parse_http_url(raw: &str) -> Option<url::Url> {
    url::Url::parse(raw).ok().filter(|u| u.scheme() == "http" || u.scheme() == "https")
}

#[derive(Debug, Clone)]
struct Settings {
    endpoint: Setting<url::Url>,
    protocol: Setting<String>,
    headers: Setting<BTreeMap<String, String>>,
}

fn settings(env: &dyn Fn(&str) -> Option<String>, prefix: &str) -> Settings {
    let name = |suffix: &str| format!("{prefix}{suffix}");
    Settings {
        endpoint: read_or_warn(
            env(&name("ENDPOINT")),
            parse_http_url,
            format!("{} is not an http or https URL, {NOT_EXPORTED}", name("ENDPOINT")),
        ),
        protocol: read_or_warn(
            env(&name("PROTOCOL")),
            |raw| {
                let lower = raw.to_lowercase();
                matches!(lower.as_str(), "http/json" | "http/protobuf").then_some(lower)
            },
            format!("{} is not http/protobuf or http/json, {NOT_EXPORTED}", name("PROTOCOL")),
        ),
        headers: read_or_warn(
            env(&name("HEADERS")),
            |raw| parse_otlp_headers(raw).ok(),
            format!(
                "{} is not a list of key=value pairs with percent-encoded values, {NOT_EXPORTED}",
                name("HEADERS")
            ),
        ),
    }
}

fn exporter(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Setting<&'static str> {
    let raw = env(name).unwrap_or_default();
    let entries: Vec<String> = raw.split(',').map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
    let mut ignored: Vec<String> = Vec::new();
    for entry in &entries {
        if entry != "otlp" && entry != "none" && !ignored.contains(entry) {
            ignored.push(entry.clone());
        }
    }
    let value = if entries.iter().any(|e| e == "otlp") {
        Some("otlp")
    } else if entries.iter().any(|e| e == "none") {
        Some("none")
    } else {
        None
    };
    let warning = (!ignored.is_empty()).then(|| {
        format!(
            "{name} names {}, which T3 Code does not export to, so {} ignored",
            ignored.join(", "),
            if ignored.len() == 1 { "it was" } else { "they were" }
        )
    });
    Setting { value, warning }
}

fn with_signal_path(signal: &str, base: &url::Url) -> url::Url {
    let mut url = base.clone();
    let path = url.path().to_owned();
    let slash = if path.ends_with('/') { "" } else { "/" };
    url.set_path(&format!("{path}{slash}v1/{}", signal.to_lowercase()));
    url
}

fn endpoint_signal(name: &str, own: &Settings, generic: &Settings, used: &mut Vec<Option<String>>) -> OtelSignal {
    let own_endpoint = own.endpoint.claimed();
    let endpoint = if own_endpoint { &own.endpoint } else { &generic.endpoint };
    used.push(endpoint.warning.clone());
    let Some(endpoint_url) = &endpoint.value else {
        return if endpoint.warning.is_none() { OtelSignal::Unset } else { OtelSignal::Off };
    };
    let protocol = if own.protocol.claimed() { &own.protocol } else { &generic.protocol };
    let headers = if own.headers.claimed() { &own.headers } else { &generic.headers };
    used.push(protocol.warning.clone());
    used.push(headers.warning.clone());
    if protocol.warning.is_some() || headers.warning.is_some() {
        return OtelSignal::Off;
    }
    let url = if own_endpoint {
        endpoint_url.clone()
    } else {
        with_signal_path(name, endpoint_url)
    };
    OtelSignal::Export {
        url: url.to_string(),
        protocol: protocol.value.clone().unwrap_or_else(|| "http/protobuf".to_owned()),
        headers: headers.value.clone(),
    }
}

/// `load`, reading variables through `env`.
pub fn load(env: &dyn Fn(&str) -> Option<String>) -> OtelEnvironment {
    let t3 = flag(env("T3CODE_OTEL_SDK_DISABLED"), T3CODE_TRUE, T3CODE_FALSE, |value| {
        format!("T3CODE_OTEL_SDK_DISABLED={value} is not a yes or a no and was ignored")
    });
    let spec = flag(env("OTEL_SDK_DISABLED"), &["true"], &["false"], |value| {
        format!(
            "OTEL_SDK_DISABLED={value} was read as false; the OpenTelemetry specification recognizes only the string true, so use OTEL_SDK_DISABLED=true or T3CODE_OTEL_SDK_DISABLED to say it any other way"
        )
    });
    let (resource_attributes, resource_warning) = match env("OTEL_RESOURCE_ATTRIBUTES") {
        None => (BTreeMap::new(), None),
        Some(raw) => match parse_resource_attributes(&raw) {
            Some(attributes) => (attributes, None),
            None => (
                BTreeMap::new(),
                Some("OTEL_RESOURCE_ATTRIBUTES is not a list of percent-encoded key=value pairs and was ignored".to_owned()),
            ),
        },
    };
    let disabled = t3.value.or(spec.value).unwrap_or(false);
    let mut used: Vec<Option<String>> = Vec::new();
    let (traces, metrics, logs) = if disabled {
        (OtelSignal::Unset, OtelSignal::Unset, OtelSignal::Unset)
    } else {
        let generic = settings(env, "OTEL_EXPORTER_OTLP_");
        let mut signal = |name: &str| {
            let exporter = exporter(env, &format!("OTEL_{name}_EXPORTER"));
            used.push(exporter.warning.clone());
            if exporter.value == Some("none") {
                return OtelSignal::Off;
            }
            let own = settings(env, &format!("OTEL_EXPORTER_OTLP_{name}_"));
            endpoint_signal(name, &own, &generic, &mut used)
        };
        (signal("TRACES"), signal("METRICS"), signal("LOGS"))
    };
    let mut warnings: Vec<String> = Vec::new();
    warnings.extend(t3.warning);
    warnings.extend(spec.warning);
    warnings.extend(resource_warning);
    // A generic variable read by several signals warns once.
    let mut seen = BTreeSet::new();
    for warning in used.into_iter().flatten() {
        if seen.insert(warning.clone()) {
            warnings.push(warning);
        }
    }
    if disabled {
        warnings.push(if t3.value == Some(true) {
            "T3CODE_OTEL_SDK_DISABLED is set, so no telemetry is exported, whatever configured it".to_owned()
        } else {
            "OTEL_SDK_DISABLED is set, so no telemetry is exported, whatever configured it; set T3CODE_OTEL_SDK_DISABLED=false to export anyway".to_owned()
        });
    }
    OtelEnvironment {
        disabled,
        warnings,
        resource_attributes,
        traces,
        metrics,
        logs,
    }
}

fn parse_resource_attributes(raw: &str) -> Option<BTreeMap<String, String>> {
    let mut attributes = BTreeMap::new();
    for pair in raw.split(',') {
        if pair.trim().is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=')?;
        let decode = |s: &str| percent_encoding::percent_decode_str(s.trim()).decode_utf8().ok().map(|c| c.into_owned());
        attributes.insert(decode(key)?, decode(value)?);
    }
    Some(attributes)
}

/// Which signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalName {
    Traces,
    Metrics,
    Logs,
}

/// How T3 Code's own variables export (`SignalExport`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalExport {
    pub protocol: String,
    pub headers: Option<BTreeMap<String, String>>,
    pub export_interval_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalEndpoint {
    pub url: String,
    pub export: SignalExport,
}

/// `resolveSignalEndpoint`.
pub fn resolve_signal_endpoint(
    otel: &OtelEnvironment,
    signal: SignalName,
    t3_url: Option<&str>,
    t3_export: &SignalExport,
    fallback_urls: &[Option<&str>],
) -> Option<SignalEndpoint> {
    if otel.disabled {
        return None;
    }
    if let Some(url) = blank_as_unset(t3_url.map(str::to_owned)) {
        return Some(SignalEndpoint {
            url,
            export: t3_export.clone(),
        });
    }
    let resolved = match signal {
        SignalName::Traces => &otel.traces,
        SignalName::Metrics => &otel.metrics,
        SignalName::Logs => &otel.logs,
    };
    match resolved {
        OtelSignal::Export { url, protocol, headers } => Some(SignalEndpoint {
            url: url.clone(),
            export: SignalExport {
                protocol: protocol.clone(),
                headers: headers.clone(),
                export_interval_ms: t3_export.export_interval_ms,
            },
        }),
        OtelSignal::Off => None,
        OtelSignal::Unset => fallback_urls
            .iter()
            .find_map(|url| blank_as_unset(url.map(str::to_owned)))
            .map(|url| SignalEndpoint {
                url,
                export: t3_export.clone(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |name: &str| map.get(name).cloned()
    }

    fn t3_export() -> SignalExport {
        SignalExport {
            protocol: "http/json".into(),
            headers: None,
            export_interval_ms: 10_000,
        }
    }

    #[test]
    fn asks_for_nothing_by_default() {
        assert_eq!(load(&env(&[])), OtelEnvironment::none());
        assert_eq!(
            resolve_signal_endpoint(&OtelEnvironment::none(), SignalName::Traces, None, &t3_export(), &[None, None]),
            None
        );
    }

    #[test]
    fn appends_the_signal_path_to_the_generic_endpoint_only() {
        let otel = load(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector.example.test:4318/base?key=1"),
            ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "https://metrics.example.test/custom"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=Bearer%20x"),
        ]));
        match &otel.traces {
            OtelSignal::Export { url, protocol, headers } => {
                assert_eq!(url, "https://collector.example.test:4318/base/v1/traces?key=1");
                assert_eq!(protocol, "http/protobuf");
                assert_eq!(headers.as_ref().unwrap()["authorization"], "Bearer x");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&otel.metrics, OtelSignal::Export { url, .. } if url == "https://metrics.example.test/custom"));
    }

    #[test]
    fn turns_signals_off_for_none_or_unreadable_settings() {
        let otel = load(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector.example.test"),
            ("OTEL_TRACES_EXPORTER", "none"),
            ("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "grpc"),
            ("OTEL_METRICS_EXPORTER", "prometheus,otlp"),
        ]));
        assert_eq!(otel.traces, OtelSignal::Off);
        assert_eq!(otel.logs, OtelSignal::Off);
        assert!(matches!(otel.metrics, OtelSignal::Export { .. }));
        assert!(otel.warnings.iter().any(|w| w.contains("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL")));
        assert!(otel.warnings.iter().any(|w| w.contains("names prometheus")));
        let bad_url = load(&env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "ftp://collector")]));
        assert_eq!(bad_url.traces, OtelSignal::Off);
        assert_eq!(bad_url.warnings.len(), 1);
    }

    #[test]
    fn the_kill_switch_wins_and_t3code_can_opt_back_in() {
        let disabled = load(&env(&[
            ("OTEL_SDK_DISABLED", "true"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://c.example.test"),
        ]));
        assert!(disabled.disabled);
        assert_eq!(disabled.traces, OtelSignal::Unset);
        assert_eq!(
            resolve_signal_endpoint(&disabled, SignalName::Traces, Some("https://t3.example.test"), &t3_export(), &[]),
            None
        );
        let opted_in = load(&env(&[("OTEL_SDK_DISABLED", "true"), ("T3CODE_OTEL_SDK_DISABLED", "no")]));
        assert!(!opted_in.disabled);
        let odd = load(&env(&[("OTEL_SDK_DISABLED", "yes")]));
        assert!(!odd.disabled);
        assert_eq!(odd.warnings.len(), 1);
    }

    #[test]
    fn resolves_t3code_variables_then_otel_then_settings() {
        let otel = load(&env(&[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "https://otel.example.test/v1/traces")]));
        let t3 = resolve_signal_endpoint(
            &otel,
            SignalName::Traces,
            Some(" https://t3.example.test "),
            &t3_export(),
            &[Some("https://settings.example.test")],
        )
        .unwrap();
        assert_eq!(t3.url, "https://t3.example.test");
        let from_otel = resolve_signal_endpoint(&otel, SignalName::Traces, None, &t3_export(), &[Some("https://settings.example.test")]).unwrap();
        assert_eq!(from_otel.url, "https://otel.example.test/v1/traces");
        assert_eq!(from_otel.export.protocol, "http/protobuf");
        let from_settings = resolve_signal_endpoint(&otel, SignalName::Logs, None, &t3_export(), &[Some(" "), Some("https://settings.example.test")]).unwrap();
        assert_eq!(from_settings.url, "https://settings.example.test");
        assert_eq!(from_settings.export, t3_export());
    }
}
