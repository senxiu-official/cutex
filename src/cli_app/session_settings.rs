use anyhow::{anyhow, Context};

use cutex::cli::args::{SessionCwdCommand, SessionDefaultsCommand};
use cutex::management::v2::session::runtime_defaults_resource;
use cutex::session::model::{normalize_runtime_token, parse_cutex_session_runtime_backend};
use cutex::session::projection::{cutex_session_cwd_summary, runtime_backend_short_label};
use cutex::session::service::{
    cutex_session_display_name, cutex_session_key_for_user_id,
    normalize_cutex_session_managed_cwd_path, persist_cutex_session_store_and_im_record,
    set_cutex_session_managed_cwd, update_cutex_session_runtime_defaults,
    CutexSessionRuntimeDefaultsPatch, CutexSessionValueUpdate,
};
use cutex::session::store::load_cutex_session_store;

use super::prompt::{
    cli_args_label, prompt_cli_args, prompt_line, prompt_optional_string, read_wizard_choice,
    wizard_value,
};
use super::session_presenter;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";

pub(crate) fn cmd_session_defaults(command: SessionDefaultsCommand) -> anyhow::Result<()> {
    match command {
        SessionDefaultsCommand::Show { id } => {
            let store = load_cutex_session_store()?;
            let key = cutex_session_key_for_user_id(&store, &id)
                .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
            let record = store.sessions.get(&key).ok_or_else(|| {
                anyhow!("cutex session disappeared while showing defaults: {key}")
            })?;
            println!(
                "{}",
                serde_json::to_string_pretty(&runtime_defaults_resource(record))?
            );
            Ok(())
        }
        SessionDefaultsCommand::Set {
            id,
            runtime_backend,
            permission_defaults,
            approval_policy,
            sandbox_mode,
            model,
            reasoning,
            cli_args,
            clear_cli_args,
        } => {
            if clear_cli_args && !cli_args.is_empty() {
                anyhow::bail!("Use either --cli-arg or --clear-cli-args, not both");
            }
            let mut store = load_cutex_session_store()?;
            let mut patch = CutexSessionRuntimeDefaultsPatch {
                runtime_backend: runtime_backend
                    .as_deref()
                    .map(parse_cutex_session_runtime_backend)
                    .transpose()?,
                permission_defaults: set_optional_string_patch(
                    permission_defaults,
                    normalize_runtime_token,
                ),
                approval_policy: set_optional_string_patch(
                    approval_policy,
                    normalize_runtime_token,
                ),
                sandbox_mode: set_optional_string_patch(sandbox_mode, normalize_runtime_token),
                model_defaults: set_optional_string_patch(model, trim_runtime_value),
                reasoning_defaults: set_optional_string_patch(reasoning, normalize_runtime_token),
                ..CutexSessionRuntimeDefaultsPatch::default()
            };
            if clear_cli_args {
                patch.default_cli_args = Some(Vec::new());
            } else if !cli_args.is_empty() {
                patch.default_cli_args = Some(cli_args);
            }
            let outcome = update_cutex_session_runtime_defaults(&mut store, &id, patch)?;
            let record = store.sessions.get(&outcome.key).ok_or_else(|| {
                anyhow!(
                    "cutex session disappeared while showing defaults: {}",
                    outcome.key
                )
            })?;
            let defaults = runtime_defaults_resource(record);
            persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
            let session_id = outcome.session_id;
            println!("{GREEN}Updated{RESET} runtime defaults for {BOLD}{session_id}{RESET}");
            println!("{}", serde_json::to_string_pretty(&defaults)?);
            Ok(())
        }
    }
}

fn set_optional_string_patch(
    value: Option<String>,
    normalize: fn(&str) -> String,
) -> CutexSessionValueUpdate<String> {
    value
        .map(|value| CutexSessionValueUpdate::set(normalize(&value)))
        .unwrap_or_default()
}

fn replace_optional_string_patch(
    value: Option<String>,
    normalize: fn(&str) -> String,
) -> CutexSessionValueUpdate<String> {
    value
        .map(|value| CutexSessionValueUpdate::set(normalize(&value)))
        .unwrap_or_else(CutexSessionValueUpdate::clear)
}

pub(crate) fn trim_runtime_value(value: &str) -> String {
    value.trim().to_string()
}

pub(crate) fn cmd_session_defaults_edit(id: &str) -> anyhow::Result<()> {
    loop {
        let store = load_cutex_session_store()?;
        let key = cutex_session_key_for_user_id(&store, id)
            .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
        let record =
            store.sessions.get(&key).cloned().ok_or_else(|| {
                anyhow!("cutex session disappeared while editing defaults: {key}")
            })?;
        println!();
        println!(
            "{BOLD}{CYAN}Runtime Defaults{RESET} {BOLD}{}{RESET}",
            cutex_session_display_name(&record)
        );
        println!(
            "  1.     runtime backend                       {}",
            wizard_value(runtime_backend_short_label(record.runtime_backend))
        );
        println!(
            "  2.     permission preset                     {}",
            wizard_value(record.permission_defaults.as_deref().unwrap_or("-"))
        );
        println!(
            "  3.     approval policy                       {}",
            wizard_value(record.approval_policy.as_deref().unwrap_or("-"))
        );
        println!(
            "  4.     sandbox mode                          {}",
            wizard_value(record.sandbox_mode.as_deref().unwrap_or("-"))
        );
        println!(
            "  5.     model                                 {}",
            wizard_value(record.model_defaults.as_deref().unwrap_or("-"))
        );
        println!(
            "  6.     reasoning                             {}",
            wizard_value(record.reasoning_defaults.as_deref().unwrap_or("-"))
        );
        println!(
            "  7.     extra CLI args                        {}",
            wizard_value(cli_args_label(&record.default_cli_args))
        );
        println!("  8.     show defaults JSON");

        let Some(choice) = read_wizard_choice(8)? else {
            println!("Done.");
            return Ok(());
        };

        let mut patch = CutexSessionRuntimeDefaultsPatch::default();
        match choice {
            1 => {
                let value = prompt_line(
                    "Runtime backend: host, native, docker, cute-alden, future",
                    runtime_backend_short_label(record.runtime_backend),
                )?;
                patch.runtime_backend = Some(parse_cutex_session_runtime_backend(&value)?);
            }
            2 => {
                patch.permission_defaults = replace_optional_string_patch(
                    prompt_optional_string(
                        "Permission preset: read-only, workspace, full-access",
                        record.permission_defaults.as_deref(),
                    )?,
                    normalize_runtime_token,
                );
            }
            3 => {
                patch.approval_policy = replace_optional_string_patch(
                    prompt_optional_string(
                        "Approval policy: on-request, never",
                        record.approval_policy.as_deref(),
                    )?,
                    normalize_runtime_token,
                );
            }
            4 => {
                patch.sandbox_mode = replace_optional_string_patch(
                    prompt_optional_string(
                        "Sandbox mode: read-only, workspace-write, danger-full-access",
                        record.sandbox_mode.as_deref(),
                    )?,
                    normalize_runtime_token,
                );
            }
            5 => {
                patch.model_defaults = replace_optional_string_patch(
                    prompt_optional_string("Model", record.model_defaults.as_deref())?,
                    trim_runtime_value,
                );
            }
            6 => {
                patch.reasoning_defaults = replace_optional_string_patch(
                    prompt_optional_string("Reasoning", record.reasoning_defaults.as_deref())?,
                    normalize_runtime_token,
                );
            }
            7 => {
                patch.default_cli_args = Some(prompt_cli_args(
                    "Extra cute-codex CLI args",
                    &record.default_cli_args,
                )?);
            }
            8 => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&runtime_defaults_resource(&record))?
                );
                continue;
            }
            _ => unreachable!(),
        }
        let mut store = load_cutex_session_store()?;
        let outcome = update_cutex_session_runtime_defaults(&mut store, &key, patch)?;
        persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
        println!("{GREEN}Saved.{RESET}");
    }
}

pub(crate) fn cmd_session_cwd(command: SessionCwdCommand) -> anyhow::Result<()> {
    match command {
        SessionCwdCommand::Show { id } => {
            let store = load_cutex_session_store()?;
            let key = cutex_session_key_for_user_id(&store, &id)
                .ok_or_else(|| anyhow!("cutex session is not known: {id}"))?;
            let record = store
                .sessions
                .get(&key)
                .ok_or_else(|| anyhow!("cutex session disappeared while showing cwd: {key}"))?;
            session_presenter::print_session_cwd_summary(record)?;
            Ok(())
        }
        SessionCwdCommand::Set { id, path } => {
            cmd_session_cwd_set(&id, Some(normalize_cutex_session_managed_cwd_path(&path)?))
        }
        SessionCwdCommand::Current { id } => {
            let cwd = std::env::current_dir()
                .context("Failed to determine current directory")?
                .display()
                .to_string();
            cmd_session_cwd_set(&id, Some(cwd))
        }
        SessionCwdCommand::Clear { id } => cmd_session_cwd_set(&id, None),
    }
}

fn cmd_session_cwd_set(id: &str, managed_cwd: Option<String>) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let outcome = set_cutex_session_managed_cwd(&mut store, id, managed_cwd)?;
    let record = store.sessions.get(&outcome.key).ok_or_else(|| {
        anyhow!(
            "cutex session disappeared while summarizing cwd: {}",
            outcome.key
        )
    })?;
    let summary = cutex_session_cwd_summary(record)?;
    persist_cutex_session_store_and_im_record(&store, &outcome.key)?;
    let session_id = outcome.session_id;
    println!("{GREEN}Updated{RESET} managed cwd for {BOLD}{session_id}{RESET}");
    session_presenter::print_session_cwd_summary_fields(&summary);
    println!("{DIM}applies to the next managed session.online launch{RESET}");
    Ok(())
}
