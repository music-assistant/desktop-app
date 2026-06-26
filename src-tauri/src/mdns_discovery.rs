use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

/// Music Assistant mDNS service type
const MA_SERVICE_TYPE: &str = "_mass._tcp.local.";

/// Select the Music Assistant server URL advertised in mDNS TXT records.
fn select_advertised_url(internal_url: Option<&str>, base_url: Option<&str>) -> Option<String> {
    internal_url
        .or(base_url)
        .map(str::trim)
        .map(|url| url.trim_end_matches('/'))
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
        .map(ToString::to_string)
}

/// Select preferred IP address from mDNS address data.
/// Prioritizes IPv4 over IPv6 and returns None if no IP is available.
fn select_preferred_ip(addresses: &[std::net::IpAddr]) -> Option<std::net::IpAddr> {
    if addresses.is_empty() {
        return None;
    }
    addresses
        .iter()
        .find(|addr| addr.is_ipv4())
        .or(addresses.first())
        .copied()
}

/// Format an IP address for use as a URL host.
fn format_ip_host(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    }
}

/// Discovered Music Assistant server
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredServer {
    /// Server name (friendly name from mDNS)
    pub name: String,
    /// Server ID (from TXT record)
    pub server_id: Option<String>,
    /// Server address (IP:port)
    pub address: String,
    /// HTTP URL to connect to
    pub url: String,
    /// Whether HTTPS is available
    pub https: bool,
}

/// Discover Music Assistant servers on the local network
/// Returns a list of discovered servers after scanning for the specified duration
pub fn discover_servers(timeout_secs: u64) -> Result<Vec<DiscoveredServer>, String> {
    let mdns = ServiceDaemon::new().map_err(|e| format!("Failed to create mDNS daemon: {}", e))?;

    let receiver = mdns
        .browse(MA_SERVICE_TYPE)
        .map_err(|e| format!("Failed to browse mDNS: {}", e))?;

    let mut servers = HashMap::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

    while std::time::Instant::now() < deadline {
        // Use a short timeout to allow checking the deadline.
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                // Extract server information.
                let name = info.get_fullname().to_string();
                let friendly_name = info
                    .get_fullname()
                    .trim_end_matches(".local.")
                    .trim_end_matches("._mass._tcp")
                    .to_string();

                // Check TXT records for additional info.
                let properties = info.get_properties();

                let server_id = properties
                    .get("server_id")
                    .or_else(|| properties.get("id"))
                    .map(|v| v.val_str().to_string());

                let advertised_url = select_advertised_url(
                    properties.get("internal_url").map(|v| v.val_str()),
                    properties.get("base_url").map(|v| v.val_str()),
                );

                let port = info.get_port();

                // Use the advertised MA URL for connecting, falling back to mDNS
                // address data only if a service does not publish the current
                // ServerInfoMessage URL fields.
                let addresses: Vec<std::net::IpAddr> =
                    info.get_addresses().iter().copied().collect();
                let ip: std::net::IpAddr = match select_preferred_ip(&addresses) {
                    Some(ip) => ip,
                    None => continue,
                };

                let host = format_ip_host(ip);
                let url = advertised_url.unwrap_or_else(|| format!("http://{}:{}", host, port));
                let https = url.starts_with("https://");
                let address = format!("{}:{}", host, port);

                let server = DiscoveredServer {
                    name: friendly_name.clone(),
                    server_id: server_id.clone(),
                    address,
                    url: url.clone(),
                    https,
                };

                // Use server_id as key if available, otherwise fullname. This helps
                // deduplicate servers responding on multiple interfaces.
                let key = server_id.clone().unwrap_or(name);
                servers.entry(key).or_insert(server);
            }
            Ok(_) | Err(flume::RecvTimeoutError::Timeout) => {}
            Err(flume::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Stop browsing while the receiver is still alive, then briefly drain the
    // expected SearchStopped event so mdns-sd does not warn about a closed channel.
    let _ = mdns.stop_browse(MA_SERVICE_TYPE);
    let stop_deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < stop_deadline {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(ServiceEvent::SearchStopped(service)) if service == MA_SERVICE_TYPE => break,
            Ok(_) | Err(flume::RecvTimeoutError::Timeout) => {}
            Err(flume::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(servers.values().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_select_advertised_url() {
        assert_eq!(
            select_advertised_url(
                Some("https://192.168.1.47:8095/"),
                Some("http://192.168.1.47:8095")
            ),
            Some("https://192.168.1.47:8095".to_string())
        );
        assert_eq!(
            select_advertised_url(None, Some(" http://192.168.1.47:8095/ ")),
            Some("http://192.168.1.47:8095".to_string())
        );
        assert_eq!(select_advertised_url(Some("192.168.1.47:8095"), None), None);
        assert_eq!(select_advertised_url(Some(""), Some("")), None);
    }

    #[test]
    fn test_format_ip_host() {
        assert_eq!(
            format_ip_host(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 47))),
            "192.168.1.47"
        );
        assert_eq!(format_ip_host(IpAddr::V6(Ipv6Addr::LOCALHOST)), "[::1]");
    }

    #[test]
    fn test_select_preferred_ip() {
        let v4 = |a, b, c, d| IpAddr::V4(Ipv4Addr::new(a, b, c, d));
        let v6_loopback = IpAddr::V6(Ipv6Addr::LOCALHOST);

        // IPv4 preferred over IPv6
        assert_eq!(
            select_preferred_ip(&[v6_loopback, v4(192, 168, 1, 1)]),
            Some(v4(192, 168, 1, 1))
        );
        // Falls back to IPv6 when no IPv4 available
        assert_eq!(select_preferred_ip(&[v6_loopback]), Some(v6_loopback));
        // No addresses → None
        assert_eq!(select_preferred_ip(&[]), None);
        // Multiple IPv4 → returns first
        assert_eq!(
            select_preferred_ip(&[v4(192, 168, 1, 1), v4(10, 0, 0, 1)]),
            Some(v4(192, 168, 1, 1))
        );
    }
}
