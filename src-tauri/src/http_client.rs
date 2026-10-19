use std::time::Duration;
use ureq::tls::{RootCerts, TlsConfig};

/// Build a native HTTP agent using the operating system's certificate verifier.
pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_uses_platform_certificate_verification_and_requested_timeout() {
        let timeout = Duration::from_secs(7);
        let agent = agent(timeout);
        let config = agent.config();
        assert!(matches!(
            config.tls_config().root_certs(),
            RootCerts::PlatformVerifier
        ));
        assert_eq!(config.timeouts().global, Some(timeout));
    }
}
