//! `EnvironmentAuthPolicy` (`auth/EnvironmentAuthPolicy.ts`): the auth descriptor a client
//! reads from `/api/auth/session` and `server.getConfig`.

use zc_contracts::{ServerAuthBootstrapMethod as Bootstrap, ServerAuthDescriptor, ServerAuthPolicy as Policy, ServerAuthSessionMethod as SessionMethod};

use crate::cookies::{is_remote_reachable_host, resolve_session_cookie_name, CookieNameInput, ServerMode};

/// `ServerAuthDescriptor` for this server.
pub fn auth_descriptor(input: &CookieNameInput) -> ServerAuthDescriptor {
    let remote = is_remote_reachable_host(input.host.as_deref());
    let policy = match (input.mode, remote) {
        (ServerMode::Desktop, false) => Policy::DesktopManagedLocal,
        (_, true) => Policy::RemoteReachable,
        (ServerMode::Web, false) => Policy::LoopbackBrowser,
    };
    let bootstrap_methods = match (policy, input.mode) {
        (Policy::DesktopManagedLocal, _) => vec![Bootstrap::DesktopBootstrap],
        (Policy::RemoteReachable, ServerMode::Desktop) => {
            vec![Bootstrap::DesktopBootstrap, Bootstrap::OneTimeToken]
        }
        _ => vec![Bootstrap::OneTimeToken],
    };
    ServerAuthDescriptor {
        policy,
        bootstrap_methods,
        session_methods: SessionMethod::ALL.to_vec(),
        session_cookie_name: resolve_session_cookie_name(input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(mode: ServerMode, host: Option<&str>, port: u16, dev: bool) -> CookieNameInput {
        CookieNameInput {
            mode,
            port,
            host: host.map(str::to_owned),
            instance_key: "/tmp/t3-auth-policy-test".into(),
            environment_id: "environment-one".into(),
            development: dev,
        }
    }

    // EnvironmentAuthPolicy.test.ts
    #[test]
    fn policies_per_mode_and_host() {
        let d = auth_descriptor(&input(ServerMode::Desktop, None, 3773, false));
        assert_eq!(d.policy, Policy::DesktopManagedLocal);
        assert_eq!(d.bootstrap_methods, vec![Bootstrap::DesktopBootstrap]);
        assert_eq!(d.session_cookie_name, "t3_session_3773");
        assert_eq!(
            auth_descriptor(&input(ServerMode::Desktop, Some("127.0.0.1"), 3774, false)).session_cookie_name,
            "t3_session_3774"
        );
        let d = auth_descriptor(&input(ServerMode::Desktop, Some("0.0.0.0"), 3773, false));
        assert_eq!(d.policy, Policy::RemoteReachable);
        assert_eq!(d.bootstrap_methods, vec![Bootstrap::DesktopBootstrap, Bootstrap::OneTimeToken]);
        let d = auth_descriptor(&input(ServerMode::Web, Some("127.0.0.1"), 3773, false));
        assert_eq!(d.policy, Policy::LoopbackBrowser);
        assert_eq!(d.bootstrap_methods, vec![Bootstrap::OneTimeToken]);
        assert!(regex::Regex::new("^t3_session_3773_[a-f0-9]{12}$").unwrap().is_match(&d.session_cookie_name));
        assert_eq!(
            d.session_methods,
            vec![
                SessionMethod::BrowserSessionCookie,
                SessionMethod::BearerAccessToken,
                SessionMethod::DpopAccessToken
            ]
        );
        let d = auth_descriptor(&input(ServerMode::Web, Some("0.0.0.0"), 3773, false));
        assert_eq!(d.policy, Policy::RemoteReachable);
        assert_eq!(d.bootstrap_methods, vec![Bootstrap::OneTimeToken]);
        assert!(regex::Regex::new("^t3_session_[a-f0-9]{12}$").unwrap().is_match(&d.session_cookie_name));
        let d = auth_descriptor(&input(ServerMode::Web, Some("0.0.0.0"), 5775, true));
        assert_eq!(d.policy, Policy::RemoteReachable);
        assert!(regex::Regex::new("^t3_session_5775_[a-f0-9]{12}$").unwrap().is_match(&d.session_cookie_name));
        let d = auth_descriptor(&input(ServerMode::Web, Some("192.168.1.50"), 3773, false));
        assert_eq!(d.policy, Policy::RemoteReachable);
    }
}
