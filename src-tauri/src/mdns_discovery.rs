use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Music Assistant mDNS service type
const MA_SERVICE_TYPE: &str = "_mass._tcp.local.";

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
                        let mut ip_from_txt: Option<std::net::IpAddr> = None;
                        if let Some(base_url) = properties.get("base_url") {
                            let url_str = base_url.val_str();
                            // Parse URL like "http://192.168.1.47:8095" to extract IP
                            let clean = url_str
                                .replace("http://", "")
                                .replace("https://", "");
                            if let Some(host_part) = clean.split(':').next() {
                                if let Ok(ip) = host_part.parse::<std::net::IpAddr>() {
                                    ip_from_txt = Some(ip);
                                }
                            }
                        }

                        let port = info.get_port();

                        // Use IP from TXT record if available, otherwise fall back to addresses
                        let ip: std::net::IpAddr = if let Some(txt_ip) = ip_from_txt {
                            txt_ip
                        } else {
                            // Get addresses from mDNS (may be aggregated from multiple hosts)
                            let addresses: Vec<std::net::IpAddr> =
                                info.get_addresses().iter().cloned().collect();

                            if addresses.is_empty() {
                                continue;
                            }

                            // Prefer IPv4 addresses over IPv6
                            *addresses
                                .iter()
                                .find(|addr| addr.is_ipv4())
                                .or(addresses.first())
                                .unwrap()
                        };

                        // Check if HTTPS is available (default to false)
                        let https = properties
                            .get("https")
                            .map(|v| v.val_str() == "true" || v.val_str() == "1")
                            .unwrap_or(false);

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
                            if !map.contains_key(&key) {
                                map.insert(key, server);
                            }
                        }
                    }
                }
                Err(flume::RecvTimeoutError::Timeout) => continue,
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
