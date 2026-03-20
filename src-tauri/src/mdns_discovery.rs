use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Music Assistant mDNS service type
const MA_SERVICE_TYPE: &str = "_mass._tcp.local.";

/// Parse IP address from a base URL
/// Supports `<http://192.168.1.47:8095>` or `<https://10.0.0.1:443>` format
/// Returns None if the URL format is invalid or contains non-parseable IP
fn parse_ip_from_base_url(url_str: &str) -> Option<std::net::IpAddr> {
    let clean = url_str.replace("http://", "").replace("https://", "");
    clean.split(':').next()?.parse::<std::net::IpAddr>().ok()
}

/// Select preferred IP address from available options
/// Prioritizes: TXT record IP > IPv4 from addresses > IPv6 from addresses
/// Returns None if no IP is available
fn select_preferred_ip(
    txt_ip: Option<std::net::IpAddr>,
    addresses: &[std::net::IpAddr],
) -> Option<std::net::IpAddr> {
    if let Some(ip) = txt_ip {
        return Some(ip);
    }
    if addresses.is_empty() {
        return None;
    }
    addresses
        .iter()
        .find(|addr| addr.is_ipv4())
        .or(addresses.first())
        .copied()
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

    let servers: Arc<Mutex<HashMap<String, DiscoveredServer>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let servers_clone = servers.clone();

    // Spawn a thread to collect discovered servers
    let handle = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

        while std::time::Instant::now() < deadline {
            // Use a short timeout to allow checking the deadline
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    if let ServiceEvent::ServiceResolved(info) = event {
                        // Extract server information
                        let name = info.get_fullname().to_string();
                        let friendly_name = info
                            .get_fullname()
                            .trim_end_matches(".local.")
                            .trim_end_matches("._mass._tcp")
                            .to_string();

                        // Check TXT records for additional info
                        let properties = info.get_properties();

                        let server_id = properties
                            .get("server_id")
                            .or_else(|| properties.get("id"))
                            .map(|v| v.val_str().to_string());

                        // Try to get the correct IP from TXT records
                        // Music Assistant may include base_url with the actual server IP
                        let ip_from_txt: Option<std::net::IpAddr> = properties
                            .get("base_url")
                            .and_then(|base_url| parse_ip_from_base_url(base_url.val_str()));

                        let port = info.get_port();

                        // Use IP from TXT record if available, otherwise fall back to addresses
                        let addresses: Vec<std::net::IpAddr> =
                            info.get_addresses().iter().copied().collect();
                        let ip: std::net::IpAddr =
                            match select_preferred_ip(ip_from_txt, &addresses) {
                                Some(ip) => ip,
                                None => continue,
                            };

                        // Check if HTTPS is available (default to false)
                        let https = properties
                            .get("https")
                            .is_some_and(|v| v.val_str() == "true" || v.val_str() == "1");

                        let protocol = if https { "https" } else { "http" };
                        let url = format!("{}://{}:{}", protocol, ip, port);
                        let address = format!("{}:{}", ip, port);

                        let server = DiscoveredServer {
                            name: friendly_name.clone(),
                            server_id: server_id.clone(),
                            address,
                            url: url.clone(),
                            https,
                        };

                        // Use server_id as key if available, otherwise fullname
                        // This helps deduplicate servers responding on multiple interfaces
                        let key = server_id.clone().unwrap_or(name);

                        if let Ok(mut map) = servers_clone.lock() {
                            // Only insert if not already present
                            map.entry(key).or_insert(server);
                        }
                    }
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    // Wait for the discovery thread to complete
    let _ = handle.join();

    // Stop browsing
    let _ = mdns.stop_browse(MA_SERVICE_TYPE);

    // Return the discovered servers
    let result: Vec<DiscoveredServer> = servers
        .lock()
        .map_err(|_| "Failed to lock servers")?
        .values()
        .cloned()
        .collect();

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_parse_ip_from_base_url() {
        let v4 = |a, b, c, d| Some(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
        let cases: Vec<(&str, Option<IpAddr>)> = vec![
            ("http://192.168.1.47:8095", v4(192, 168, 1, 47)),
            ("https://10.0.0.1:443", v4(10, 0, 0, 1)),
            ("192.168.1.100:8095", v4(192, 168, 1, 100)),
            ("http://[::1]:8095", None), // Brackets don't parse as bare IpAddr
            ("http://not_an_ip:8095", None),
            ("", None),
            ("http://", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_ip_from_base_url(input), expected, "input: {input}");
        }
    }

    #[test]
    fn test_select_preferred_ip() {
        let v4 = |a, b, c, d| IpAddr::V4(Ipv4Addr::new(a, b, c, d));
        let v6_loopback = IpAddr::V6(Ipv6Addr::LOCALHOST);

        // TXT IP takes precedence over mDNS addresses
        assert_eq!(
            select_preferred_ip(Some(v4(192, 168, 1, 1)), &[v4(10, 0, 0, 1)]),
            Some(v4(192, 168, 1, 1))
        );
        // IPv4 preferred over IPv6
        assert_eq!(
            select_preferred_ip(None, &[v6_loopback, v4(192, 168, 1, 1)]),
            Some(v4(192, 168, 1, 1))
        );
        // Falls back to IPv6 when no IPv4 available
        assert_eq!(select_preferred_ip(None, &[v6_loopback]), Some(v6_loopback));
        // No addresses and no TXT → None
        assert_eq!(select_preferred_ip(None, &[]), None);
        // Multiple IPv4 → returns first
        assert_eq!(
            select_preferred_ip(None, &[v4(192, 168, 1, 1), v4(10, 0, 0, 1)]),
            Some(v4(192, 168, 1, 1))
        );
    }
}
