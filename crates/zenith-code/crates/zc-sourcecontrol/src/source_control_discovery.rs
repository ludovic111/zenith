//! `SourceControlDiscovery.ts`: what `server.discoverSourceControl` answers: the version
//! control systems (git, and jj which is not implemented) and every source control provider.

use zc_contracts::{EOption, SourceControlDiscoveryResult, SourceControlDiscoveryStatus, VcsDiscoveryItem, VcsDriverKind};
use zc_core::vcs_process::{VcsProcess, VcsProcessInput};

use crate::discovery::{detail_from_cause, first_non_empty_line};
use crate::registry::SourceControlProviderRegistry;

struct VcsProbe {
    kind: VcsDriverKind,
    label: &'static str,
    executable: &'static str,
    implemented: bool,
    install_hint: &'static str,
}

const VCS_PROBES: [VcsProbe; 2] = [
    VcsProbe {
        kind: VcsDriverKind::Git,
        label: "Git",
        executable: "git",
        implemented: true,
        install_hint: "Install Git from https://git-scm.com/downloads or with your package manager.",
    },
    VcsProbe {
        kind: VcsDriverKind::Jj,
        label: "Jujutsu",
        executable: "jj",
        implemented: false,
        install_hint: "Install Jujutsu with `brew install jj` or from https://github.com/jj-vcs/jj.",
    },
];

/// The `SourceControlDiscovery` service.
#[derive(Clone)]
pub struct SourceControlDiscovery {
    process: VcsProcess,
    providers: SourceControlProviderRegistry,
    cwd: String,
}

impl SourceControlDiscovery {
    /// `cwd` is the server's working directory (`ServerConfig.cwd`).
    pub fn new(process: VcsProcess, providers: SourceControlProviderRegistry, cwd: impl Into<String>) -> Self {
        Self {
            process,
            providers,
            cwd: cwd.into(),
        }
    }

    async fn probe(&self, probe: &VcsProbe) -> VcsDiscoveryItem {
        let mut input = VcsProcessInput::new("source-control.discovery.probe", probe.executable, ["--version"], self.cwd.as_str());
        input.timeout_ms = Some(5_000);
        input.max_output_bytes = Some(8_000);
        input.append_truncation_marker = true;
        let (status, version, detail) = match self.process.run(input).await {
            Ok(output) => (
                SourceControlDiscoveryStatus::Available,
                first_non_empty_line(&output.stdout).or_else(|| first_non_empty_line(&output.stderr)),
                None,
            ),
            Err(error) => (SourceControlDiscoveryStatus::Missing, None, detail_from_cause(&error)),
        };
        VcsDiscoveryItem {
            kind: probe.kind,
            implemented: probe.implemented,
            label: probe.label.into(),
            executable: Some(probe.executable.into()),
            status,
            version: EOption(version),
            install_hint: probe.install_hint.into(),
            detail: EOption(detail),
        }
    }

    /// `discover`.
    pub async fn discover(&self) -> SourceControlDiscoveryResult {
        let vcs = futures::future::join_all(VCS_PROBES.iter().map(|probe| self.probe(probe)));
        let (version_control_systems, source_control_providers) = futures::join!(vcs, self.providers.discover());
        SourceControlDiscoveryResult {
            version_control_systems,
            source_control_providers,
        }
    }
}
