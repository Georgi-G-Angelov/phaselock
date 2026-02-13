use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

// ── Constants ───────────────────────────────────────────────────────────────

/// mDNS service type for PhaseLock sessions.
const SERVICE_TYPE: &str = "_phaselock._tcp.local.";

// ── MdnsBroadcaster (host side) ────────────────────────────────────────────

/// Advertises a PhaseLock session via mDNS so peers on the LAN can discover it.
pub struct MdnsBroadcaster {
    daemon: ServiceDaemon,
    service_fullname: String,
}

impl MdnsBroadcaster {
    /// Register a new session on the local network.
    pub fn register(
        session_name: &str,
        host_display_name: &str,
        port: u16,
        peer_count: u8,
        max_peers: u8,
    ) -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;

        let txt_records: &[(&str, &str)] = &[
            ("name", session_name),
            ("host", host_display_name),
            ("peers", &peer_count.to_string()),
            ("max_peers", &max_peers.to_string()),
        ];

        // Use a sanitised instance name (mDNS instance names have restrictions).
        let instance_name = sanitize_instance_name(session_name);

        let service_info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &format!("{instance_name}.local."),
            "", // empty = auto-detect addresses
            port,
            txt_records,
        )?
        .enable_addr_auto();

        let service_fullname = service_info.get_fullname().to_owned();

        daemon.register(service_info)?;
        log::info!("mDNS: registered session \"{session_name}\" as {service_fullname}");

        Ok(Self {
            daemon,
            service_fullname,
        })
    }

    /// Update the peer count in the TXT record (re-register with new metadata).
    pub fn update_peer_count(
        &self,
        session_name: &str,
        host_display_name: &str,
        port: u16,
        peer_count: u8,
        max_peers: u8,
    ) -> Result<(), mdns_sd::Error> {
        let txt_records: &[(&str, &str)] = &[
            ("name", session_name),
            ("host", host_display_name),
            ("peers", &peer_count.to_string()),
            ("max_peers", &max_peers.to_string()),
        ];

        let instance_name = sanitize_instance_name(session_name);

        let service_info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &format!("{instance_name}.local."),
            "",
            port,
            txt_records,
        )?
        .enable_addr_auto();

        self.daemon.register(service_info)?;
        log::debug!("mDNS: updated peer count to {peer_count}");
        Ok(())
    }

    /// Unregister the service (call when the session ends).
    pub fn unregister(self) -> Result<(), mdns_sd::Error> {
        log::info!("mDNS: unregistering {}", self.service_fullname);
        let receiver = self.daemon.unregister(&self.service_fullname)?;
        // Wait briefly for confirmation, but don't block forever.
        let _ = receiver.recv_timeout(std::time::Duration::from_secs(2));
        self.daemon.shutdown()?;
        Ok(())
    }
}

// ── MdnsBrowser (peer side) ────────────────────────────────────────────────

/// A PhaseLock session discovered on the local network.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    pub session_name: String,
    pub host_name: String,
    pub address: SocketAddr,
    pub peer_count: u8,
    pub max_peers: u8,
    /// mDNS full service name (used internally to track removals).
    pub fullname: String,
}

/// Browses the local network for PhaseLock sessions via mDNS.
pub struct MdnsBrowser {
    daemon: ServiceDaemon,
    sessions: Arc<Mutex<HashMap<String, DiscoveredSession>>>,
    _browse_handle: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl MdnsBrowser {
    /// Start browsing for PhaseLock sessions on the local network.
    pub fn start() -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let receiver = daemon.browse(SERVICE_TYPE)?;

        let sessions: Arc<Mutex<HashMap<String, DiscoveredSession>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let sess = sessions.clone();
        let flag = stop_flag.clone();

        let browse_handle = std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                match receiver.recv_timeout(std::time::Duration::from_millis(200)) {
                    Ok(event) => match event {
                        ServiceEvent::ServiceResolved(info) => {
                            let fullname = info.get_fullname().to_owned();

                            // Extract first usable IP address.
                            let ip: Option<IpAddr> = info
                                .get_addresses_v4()
                                .into_iter()
                                .next()
                                .map(|v4| IpAddr::V4(*v4));

                            let addr = match ip {
                                Some(ip) => SocketAddr::new(ip, info.get_port()),
                                None => continue, // skip if no address resolved
                            };

                            let session_name = info
                                .get_property_val_str("name")
                                .unwrap_or("")
                                .to_owned();
                            let host_name = info
                                .get_property_val_str("host")
                                .unwrap_or("")
                                .to_owned();
                            let peer_count: u8 = info
                                .get_property_val_str("peers")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            let max_peers: u8 = info
                                .get_property_val_str("max_peers")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(5);

                            log::info!(
                                "mDNS: discovered session \"{session_name}\" by {host_name} at {addr}"
                            );

                            sess.lock().insert(
                                fullname.clone(),
                                DiscoveredSession {
                                    session_name,
                                    host_name,
                                    address: addr,
                                    peer_count,
                                    max_peers,
                                    fullname,
                                },
                            );
                        }
                        ServiceEvent::ServiceRemoved(_ty, fullname) => {
                            log::info!("mDNS: session removed: {fullname}");
                            sess.lock().remove(&fullname);
                        }
                        _ => {}
                    },
                    Err(e) => {
                        if e.to_string().contains("disconnected") {
                            break;
                        }
                        // Timeout — just loop again.
                        continue;
                    }
                }
            }
        });

        log::info!("mDNS: browsing for PhaseLock sessions");

        Ok(Self {
            daemon,
            sessions,
            _browse_handle: Some(browse_handle),
            stop_flag,
        })
    }

    /// Get a snapshot of currently discovered sessions.
    pub fn get_sessions(&self) -> Vec<DiscoveredSession> {
        self.sessions.lock().values().cloned().collect()
    }

    /// Stop browsing and shut down the mDNS daemon.
    pub fn stop(self) -> Result<(), mdns_sd::Error> {
        self.stop_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        self.daemon.shutdown()?;
        log::info!("mDNS: browser stopped");
        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Sanitize a session name for use as an mDNS instance name.
/// mDNS instance names can be up to 63 bytes of UTF-8 and should avoid dots.
fn sanitize_instance_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '.' { '-' } else { c })
        .collect();
    if cleaned.len() > 63 {
        cleaned[..63].to_string()
    } else {
        cleaned
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_sanitize_instance_name() {
        assert_eq!(sanitize_instance_name("My Session"), "My Session");
        assert_eq!(sanitize_instance_name("With.Dots"), "With-Dots");
        let long = "A".repeat(100);
        assert_eq!(sanitize_instance_name(&long).len(), 63);
    }

    #[test]
    fn test_register_and_discover_session() {
        // Register a session.
        let broadcaster =
            MdnsBroadcaster::register("Test Session", "Alice", 17401, 0, 5).unwrap();

        // Browse for sessions.
        let browser = MdnsBrowser::start().unwrap();

        // Wait for discovery (mDNS can take a few seconds on loopback).
        let mut found = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(500));
            let sessions = browser.get_sessions();
            if let Some(s) = sessions.iter().find(|s| s.session_name == "Test Session") {
                assert_eq!(s.host_name, "Alice");
                assert_eq!(s.address.port(), 17401);
                assert_eq!(s.peer_count, 0);
                assert_eq!(s.max_peers, 5);
                found = true;
                break;
            }
        }
        assert!(found, "Should have discovered the session");

        // Clean up.
        let _ = broadcaster.unregister();
        let _ = browser.stop();
    }

    #[test]
    fn test_unregister_removes_session() {
        let broadcaster =
            MdnsBroadcaster::register("Vanish Session", "Bob", 17401, 1, 5).unwrap();

        let browser = MdnsBrowser::start().unwrap();

        // Wait for discovery.
        let mut found = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(500));
            if browser
                .get_sessions()
                .iter()
                .any(|s| s.session_name == "Vanish Session")
            {
                found = true;
                break;
            }
        }
        assert!(found, "Should have discovered the session");

        // Unregister.
        let _ = broadcaster.unregister();

        // Wait for removal.
        let mut removed = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(500));
            if !browser
                .get_sessions()
                .iter()
                .any(|s| s.session_name == "Vanish Session")
            {
                removed = true;
                break;
            }
        }
        assert!(removed, "Session should have been removed after unregister");

        let _ = browser.stop();
    }

    #[test]
    fn test_discover_two_sessions() {
        let b1 =
            MdnsBroadcaster::register("Session Alpha", "Host1", 17401, 0, 5).unwrap();
        let b2 =
            MdnsBroadcaster::register("Session Beta", "Host2", 17402, 2, 5).unwrap();

        let browser = MdnsBrowser::start().unwrap();

        let mut found_alpha = false;
        let mut found_beta = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(500));
            let sessions = browser.get_sessions();
            if sessions.iter().any(|s| s.session_name == "Session Alpha") {
                found_alpha = true;
            }
            if sessions.iter().any(|s| s.session_name == "Session Beta") {
                found_beta = true;
            }
            if found_alpha && found_beta {
                break;
            }
        }

        assert!(found_alpha, "Should have discovered Session Alpha");
        assert!(found_beta, "Should have discovered Session Beta");

        let _ = b1.unregister();
        let _ = b2.unregister();
        let _ = browser.stop();
    }
}
