//! Service targets cannot tunnel into cutex's local control plane. Check both
//! registration and every dial, including legacy registry entries.
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

static ACTIVE: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
pub fn note(port: u16) {
    if port > 0 {
        if let Ok(mut ports) = ACTIVE.get_or_init(Mutex::default).lock() {
            ports.insert(port);
        }
    }
}
pub fn forbidden(port: u16) -> bool {
    if port == 0 {
        return false;
    }
    if [24270, 24260, 24250, 25370].contains(&port) {
        return true;
    }
    if ACTIVE
        .get_or_init(Mutex::default)
        .lock()
        .map_or(true, |ports| ports.contains(&port))
    {
        return true;
    }
    let config = crate::config::store::load_codez_config();
    if port == crate::management::service::management_port(&config)
        || port == crate::agent_bus::service::agent_bus_port(&config)
    {
        return true;
    }
    let default = crate::management::listeners::default_listen_port();
    if port == default
        || config.management_listen.as_ref().is_some_and(|list| {
            list.iter()
                .filter_map(|s| crate::management::listeners::ListenSpec::parse(s, default).ok())
                .any(|s| s.port() == port)
        })
    {
        return true;
    }
    crate::management::relay::listener_ports().contains(&port)
}
pub fn check(port: u16) -> super::store::ServicesResult<()> {
    if forbidden(port) {
        return Err(super::store::ServicesError::new(
            403,
            "service_control_port_forbidden",
            "services cannot target cutex control ports",
        ));
    }
    Ok(())
}
