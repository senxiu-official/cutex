//! Runtime override policy for one-off profile launches.

use crate::launch::docker::{
    default_docker_user_name, docker_user_name, normalize_docker_user_name,
};
use crate::profiles::model::{RuntimeConfig, StoredAccount};

pub fn apply_runtime_override(
    account: &StoredAccount,
    force_host: bool,
    docker_image: Option<String>,
    docker_user_name: Option<String>,
) -> anyhow::Result<StoredAccount> {
    if force_host {
        let mut effective = account.clone();
        effective.runtime = RuntimeConfig::Host;
        return Ok(effective);
    }

    if let Some(image) = docker_image {
        let mut effective = account.clone();
        effective.runtime = RuntimeConfig::Docker {
            image,
            user_name: Some(normalize_docker_user_name(docker_user_name)?),
        };
        return Ok(effective);
    }

    Ok(account.clone())
}

pub fn runtime_description(runtime: &RuntimeConfig) -> String {
    match runtime {
        RuntimeConfig::Host => "host".to_string(),
        RuntimeConfig::Docker { image, user_name } => format!(
            "docker image={} user={}",
            image,
            docker_user_name(user_name.as_deref()).unwrap_or_else(|_| default_docker_user_name())
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::{CliKind, StoredAccount};

    fn sample_account(runtime: RuntimeConfig) -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            auto_pick: false,
            claude_code: Default::default(),
            variants: Default::default(),
            id: "account-id".to_string(),
            name: "demo".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: None,
            runtime,
            proxy: None,
            session: None,
            cli_kind: CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
            interface_base_urls: Default::default(),
        }
    }

    #[test]
    fn runtime_override_can_force_host_from_docker_profile() {
        let account = sample_account(RuntimeConfig::Docker {
            image: "cutex-dev".to_string(),
            user_name: Some("dev".to_string()),
        });

        let effective = apply_runtime_override(&account, true, None, None)
            .expect("force-host override should apply");

        assert_eq!(effective.runtime, RuntimeConfig::Host);
        assert_eq!(account.name, effective.name);
    }

    #[test]
    fn runtime_override_can_select_docker_runtime() {
        let account = sample_account(RuntimeConfig::Host);

        let effective = apply_runtime_override(
            &account,
            false,
            Some("cutex-dev".to_string()),
            Some("devuser".to_string()),
        )
        .expect("docker override should apply");

        assert_eq!(
            effective.runtime,
            RuntimeConfig::Docker {
                image: "cutex-dev".to_string(),
                user_name: Some("devuser".to_string()),
            }
        );
    }

    #[test]
    fn runtime_description_matches_launch_output_contract() {
        assert_eq!(runtime_description(&RuntimeConfig::Host), "host");
        assert_eq!(
            runtime_description(&RuntimeConfig::Docker {
                image: "cutex-dev".to_string(),
                user_name: Some("dev".to_string()),
            }),
            "docker image=cutex-dev user=dev"
        );
    }
}
