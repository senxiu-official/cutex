//! `cutex upgrade …` and `cutex service …` (docs/backend-upgrade.md).

use std::time::Duration;

use anyhow::{bail, Context};
use cutex::cli::args::{ServiceCommand, UpgradeCommand};
use cutex::upgrade::control::{self, ApplyRequest, RollbackRequest};
use cutex::upgrade::layout::Layout;
use cutex::upgrade::manifest::Mode;
use cutex::upgrade::service::{self, InstallOptions, RealExec};
use cutex::upgrade::stage::{self, LocalRuntimes, UpgradeError};
use cutex::upgrade::state::{self, OperationLock, PackageRun, Phase, Settings, State};

fn layout() -> anyhow::Result<Layout> {
    Layout::detect().with_context(|| {
        format!(
            "this cutex does not run from a managed install (set {} or run `cutex service install`)",
            cutex::upgrade::layout::INSTALL_BASE_ENV
        )
    })
}

fn print(value: &serde_json::Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn explain(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<UpgradeError>() {
        Some(e) if !e.details.as_object().is_none_or(|d| d.is_empty()) => anyhow::anyhow!("{} ({})", e, e.details),
        _ => error,
    }
}

fn package_run(text: Option<String>) -> anyhow::Result<Option<PackageRun>> {
    text.map(|t| PackageRun::parse(&t).with_context(|| format!("--package-run {t}: after, during or none"))).transpose()
}

/// Print phase changes until the run is terminal.
fn follow(layout: &Layout, run_id: &str) -> anyhow::Result<()> {
    let mut seen = 0;
    loop {
        if let Some(run) = state::read_run(layout, run_id) {
            for entry in run.timeline.iter().skip(seen) {
                println!("{}  {}{}", entry.at, entry.phase.as_str(), entry.detail.as_deref().map(|d| format!("  {d}")).unwrap_or_default());
            }
            seen = run.timeline.len();
            if run.phase.terminal() {
                if let Some(owner) = &run.needs_owner {
                    println!("needs owner: {}", owner.reason);
                    for command in &owner.commands {
                        println!("  {command}");
                    }
                }
                if let Some(error) = &run.error {
                    println!("error {}: {}", error.code, error.message);
                }
                if matches!(run.phase, Phase::Committed) {
                    return Ok(());
                }
                bail!("run {run_id} ended {}", run.phase.as_str());
            }
        }
        if control::reconcile_lost_run(layout).is_some_and(|r| r.run_id == run_id && r.phase.terminal()) {
            continue;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub(super) fn run_upgrade(command: UpgradeCommand) -> anyhow::Result<()> {
    let keys = cutex::upgrade::keys::trusted_keys();
    match command {
        UpgradeCommand::Selftest { json } => {
            let result = control::selftest();
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                print(&result)?;
            }
            if result["ok"] != true && !json {
                bail!("self test failed");
            }
            Ok(())
        }
        UpgradeCommand::Status { json } => {
            // Unmanaged: never a stand-in base (see `upgrade_api::layout`).
            let view = Layout::detect().map_or_else(control::unmanaged_status_view, |layout| control::status_view(&layout));
            if json {
                return print(&view);
            }
            if view["managed"] != true {
                println!("unmanaged: this cutex was deployed by hand; `cutex upgrade` does not change it");
                return Ok(());
            }
            println!("installed  {}", view["installed"]["release"].as_str().unwrap_or("?"));
            println!("previous   {}", view["previousRelease"].as_str().unwrap_or("-"));
            println!("available  {}", view["available"]["release"].as_str().unwrap_or("-"));
            println!("staged     {}", view["staged"]["release"].as_str().unwrap_or("-"));
            if let Some(run) = view["run"].as_object() {
                println!("run        {} {} {} → {} ({})", run["runId"].as_str().unwrap_or(""), run["kind"].as_str().unwrap_or(""),
                    run["fromRelease"].as_str().unwrap_or(""), run["toRelease"].as_str().unwrap_or(""), run["phase"].as_str().unwrap_or(""));
            }
            if view["trustedKeys"].as_array().is_some_and(Vec::is_empty) {
                println!("note: this build trusts no release signing key; check / stage are unavailable");
            }
            Ok(())
        }
        UpgradeCommand::Check => {
            let layout = layout()?;
            let settings = Settings::load(&layout);
            let available = stage::check(&layout, &settings.source()?, &keys, settings.channel(&layout), &LocalRuntimes::read()).map_err(explain)?;
            match available {
                Some(available) => print(&serde_json::to_value(available)?),
                None => {
                    println!("up to date");
                    Ok(())
                }
            }
        }
        UpgradeCommand::Stage { release } => {
            let layout = layout()?;
            let release = release
                .or_else(|| State::load(&layout).available.map(|a| a.release))
                .context("no release given and none available (run `cutex upgrade check`)")?;
            let _lock = OperationLock::try_acquire(&layout)?.context("another upgrade operation is running")?;
            let settings = Settings::load(&layout);
            let staged = stage::stage(&layout, &settings.source()?, &keys, settings.channel(&layout), &release, &LocalRuntimes::read())
                .map_err(explain)?;
            print(&serde_json::to_value(&staged)?)?;
            if !staged.preflight_ok {
                bail!("preflight failed");
            }
            Ok(())
        }
        UpgradeCommand::Apply { release, mode, max_wait_secs, confirm_one_way, package_run: package, no_follow } => {
            let layout = layout()?;
            let mode = match mode.as_deref() {
                None => None,
                Some("reload") => Some(Mode::Reload),
                Some("full") => Some(Mode::Full),
                Some(other) => bail!("--mode {other}: reload or full"),
            };
            let request = ApplyRequest {
                release,
                mode,
                max_wait_secs,
                confirm_one_way,
                package_run: package_run(package)?,
                requested_by: Some("cli".into()),
            };
            let run = control::prepare_apply(&layout, &keys, &request).map_err(explain)?;
            let run = control::start(&layout, &keys, run)?;
            println!(
                "run {} {} → {} ({} mode, {})",
                run.run_id,
                run.from_release,
                run.to_release,
                serde_json::to_value(run.mode)?.as_str().unwrap_or(""),
                serde_json::to_value(run.class)?.as_str().unwrap_or("")
            );
            if no_follow {
                return Ok(());
            }
            follow(&layout, &run.run_id)
        }
        UpgradeCommand::Cancel => {
            let layout = layout()?;
            let run = control::cancel(&layout).map_err(explain)?;
            println!("cancel requested for {} ({})", run.run_id, run.phase.as_str());
            Ok(())
        }
        UpgradeCommand::Rollback { to, dry_run, max_wait_secs, no_follow } => {
            let layout = layout()?;
            let request = RollbackRequest { to, max_wait_secs, dry_run, requested_by: Some("cli".into()) };
            let (run, preview) = control::prepare_rollback(&layout, &request).map_err(explain)?;
            print(&preview)?;
            let Some(run) = run else { return Ok(()) };
            let run = control::start(&layout, &keys, run)?;
            println!("run {} {} → {}", run.run_id, run.from_release, run.to_release);
            if no_follow {
                return Ok(());
            }
            follow(&layout, &run.run_id)
        }
        UpgradeCommand::History { limit } => {
            let layout = layout()?;
            for run in state::history(&layout, limit) {
                println!(
                    "{}  {}  {:8}  {} → {}  {}{}",
                    run.created_at,
                    run.run_id,
                    serde_json::to_value(run.kind)?.as_str().unwrap_or(""),
                    run.from_release,
                    run.to_release,
                    run.phase.as_str(),
                    run.error.as_ref().map(|e| format!("  ({})", e.code)).unwrap_or_default()
                );
            }
            Ok(())
        }
        UpgradeCommand::Config {
            channel,
            source,
            clear_source,
            check_interval_hours,
            auto_stage,
            auto_apply,
            package_run: package,
            package_run_batch,
            max_wait_secs,
        } => {
            let layout = layout()?;
            let mut settings = Settings::load(&layout);
            let mut changed = false;
            if let Some(channel) = channel {
                settings.channel = cutex::build_info::normalize_channel(&channel).with_context(|| format!("channel {channel}: release or main"))?.into();
                changed = true;
            }
            if let Some(source) = source {
                cutex::upgrade::source::Source::parse(&source)?;
                settings.source = Some(source);
                changed = true;
            }
            if clear_source {
                settings.source = None;
                changed = true;
            }
            if let Some(hours) = check_interval_hours {
                settings.check_interval_hours = hours;
                changed = true;
            }
            if let Some(value) = auto_stage {
                settings.auto_stage = value;
                changed = true;
            }
            if let Some(value) = auto_apply {
                settings.auto_apply = value;
                changed = true;
            }
            if let Some(value) = package_run(package)? {
                settings.package_run = value;
                changed = true;
            }
            if let Some(batch) = package_run_batch {
                anyhow::ensure!(batch >= 1, "--package-run-batch must be at least 1");
                settings.package_run_batch = batch;
                changed = true;
            }
            if let Some(wait) = max_wait_secs {
                anyhow::ensure!(wait <= state::MAX_MAX_WAIT_SECS, "--max-wait must be at most {}", state::MAX_MAX_WAIT_SECS);
                settings.max_wait_secs = wait;
                changed = true;
            }
            if changed {
                settings.save(&layout)?;
            }
            print(&settings.view(&layout))
        }
        UpgradeCommand::Run { run_id } => {
            let layout = layout()?;
            let run = control::execute(&layout, &run_id)?;
            eprintln!("cutex upgrade: {} finished {}", run.run_id, run.phase.as_str());
            Ok(())
        }
    }
}

pub(super) fn run_service(command: ServiceCommand) -> anyhow::Result<()> {
    match command {
        ServiceCommand::Install { base, unit_dir, channel, no_start, dry_run, force } => {
            let base = match base {
                Some(base) => base,
                None => cutex::upgrade::layout::default_base().context("cannot determine the install base; pass --base")?,
            };
            let exe = std::env::current_exe()?;
            let options = InstallOptions {
                base,
                unit_dir,
                channel: channel.unwrap_or_else(|| cutex::build_info::channel().to_string()),
                no_start,
                dry_run,
                force,
                source_dir: exe.parent().context("executable has no directory")?.to_path_buf(),
                release: service::build_release_label(),
                names: service::ServiceNames::for_install()?,
            };
            let actions = service::install(&options, &RealExec)?;
            for action in &actions {
                println!("{}{action}", if dry_run { "would " } else { "" });
            }
            if !dry_run {
                if let Some(hint) = service::linger_hint(&RealExec) {
                    println!("note: {hint}");
                }
            }
            Ok(())
        }
        ServiceCommand::Uninstall { dry_run } => {
            let layout = layout()?;
            for action in service::uninstall(&layout, dry_run, &RealExec)? {
                println!("{}{action}", if dry_run { "would " } else { "" });
            }
            Ok(())
        }
        ServiceCommand::Status => {
            let layout = Layout::detect()
                .or_else(Layout::default_layout)
                .context("cannot determine the install base")?;
            print(&service::status(&layout, &RealExec))
        }
        ServiceCommand::Run => cutex::upgrade::supervisor::run(&layout()?),
    }
}
