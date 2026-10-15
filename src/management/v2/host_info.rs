//! `GET /v2/host`: who this daemon is, in one call (capability
//! `host-identity`; cutex-desktop docs/research/multi-host-rfc-20261009.md
//! §14 items 2–3).
//!
//! - `hostId`: machine name (shared by every cutex home on the machine);
//! - `instanceId`: random id created once per cutex home
//!   (`<cutex home>/instance-id`, see `management::instance`), so a client
//!   sees the same daemon added twice and a daemon being replaced;
//! - `platform` / `arch`: `std::env::consts::{OS, ARCH}`;
//! - `osVersion`: short human OS version, null when unknown;
//! - `cutexBuild`: same object as `/v2/capabilities` `cutexBuild`;
//! - `serviceManager` `{kind, unit, managedInstall}` (backend auto-update RFC
//!   §9 P0): `kind` = `systemd` | `cutex-supervisor` | `runtime-host` |
//!   `none`; `managedInstall` = the daemon runs from a `cutex service
//!   install` layout (only those can `cutex upgrade`);
//! - `runtimes` (owner only; null when the stores cannot be read): native /
//!   Claude runtime selected for new agents + how many agents use each
//!   (`runtimes_api::summary_from`).

use serde_json::{json, Value};

pub fn host_resource() -> Value {
    json!({
        "schema": "cutex/host/v1",
        "hostId": crate::platform::host::current_host_name(),
        "instanceId": crate::management::instance::instance_id(),
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "osVersion": crate::platform::host::os_version(),
        "cutexBuild": crate::build_info::build_json(),
        "executable": std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        "serviceManager": crate::upgrade::service_manager::host_view(),
        // Agent counts are the owner's; a non-owner (HostRead) sees null.
        "runtimes": if crate::management::users::current_principal().is_some_and(|p| !p.is_owner()) {
            Value::Null
        } else {
            super::runtimes_api::summary().unwrap_or(Value::Null)
        },
    })
}

pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "host",
    title: "Host identity",
    routes: &["GET /v2/host"],
    text: include_str!("schema/export/host.json"),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_resource_carries_identity_fields() {
        let value = host_resource();
        assert_eq!(value["schema"], "cutex/host/v1");
        assert_eq!(value["instanceId"], "ci-test-instance");
        assert_eq!(value["instanceId"], crate::management::v2::capabilities::capabilities_resource()["instanceId"]);
        assert_eq!(value["platform"], std::env::consts::OS);
        assert_eq!(value["arch"], std::env::consts::ARCH);
        assert_eq!(value["cutexBuild"], crate::build_info::build_json());
        assert_eq!(value["cutexBuild"], crate::management::v2::capabilities::capabilities_resource()["cutexBuild"]);
        assert!(value["osVersion"].is_string() || value["osVersion"].is_null());
        assert!(value["hostId"].as_str().is_some_and(|id| !id.is_empty()));
        assert!(["systemd", "cutex-supervisor", "runtime-host", "none"]
            .contains(&value["serviceManager"]["kind"].as_str().unwrap()));
        assert!(value["serviceManager"]["managedInstall"].is_boolean());
        super::super::schema_export::assert_valid("host", "host", &value);
        let mut with_runtimes = value.clone();
        with_runtimes["runtimes"] = json!({
            "native": {"selected": "0.162.1-r53", "latest": "0.162.1-r53",
                "inUse": [{"release": "r53", "version": "0.162.1-r53", "agents": 3, "selected": true, "missing": false}]},
            "claude": {"selected": null, "inUse": []},
            "unpinnedAgents": 0,
        });
        super::super::schema_export::assert_valid("host", "host", &with_runtimes);
    }

    #[test]
    fn host_schema_accepts_unknown_os_version_and_rejects_missing_identity() {
        let mut value = host_resource();
        value["osVersion"] = Value::Null;
        super::super::schema_export::assert_valid("host", "host", &value);
        value.as_object_mut().unwrap().remove("instanceId");
        let validator = super::super::schema_export::validator("host", "host").unwrap();
        assert!(!validator.is_valid(&value));
    }
}
