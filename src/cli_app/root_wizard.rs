use cutex::cli::args::SessionListArgs;
use cutex::config::paths::{host_codex_home_dir, migrate_legacy_runtime_layout};

use super::launch_output::LaunchOutput;
use super::prompt::read_wizard_choice;

const CODEZ_BUILD: &str = "2026-07-21-quick-resume-takeover-v186";

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";

pub(crate) fn print_cutex_build(output: LaunchOutput) {
    output.line(format_args!("cutex build: {CODEZ_BUILD}"));
}

/// `migrate`: run the one-time legacy layout move first (false for the
/// read-only probes, see `app::skips_startup_migrations`).
pub(crate) fn set_codez_codex_home(migrate: bool) -> anyhow::Result<()> {
    if migrate {
        migrate_legacy_runtime_layout()?;
    }
    let path = host_codex_home_dir()?;
    std::env::set_var("CODEX_HOME", &path);
    Ok(())
}

pub(crate) fn cmd_wizard() -> anyhow::Result<()> {
    loop {
        println!();
        println!("{BOLD}{CYAN}cutex Wizard{RESET}");
        println!("  1. Start / resume / attach managed session");
        println!("  2. Start new throwaway session");
        println!("  3. List profiles");
        println!("  4. Show active profile");
        println!("  5. Edit active profile");
        println!("  6. Edit global settings");
        println!("  7. Manage sessions");
        println!("  8. Log in / create profile");

        let Some(choice) = read_wizard_choice(8)? else {
            println!("Done.");
            return Ok(());
        };

        match choice {
            1 => return super::session::start_wizard(&SessionListArgs::default()),
            2 => {
                return super::launch::quick_run(
                    Vec::new(),
                    LaunchOutput::Human,
                    true,
                    false,
                    false,
                    Vec::new(),
                )
            }
            3 => super::profile::profile_list()?,
            4 => super::profile::profile_show(None)?,
            5 => super::profile::cmd_profile_edit(None)?,
            6 => super::settings::cmd_global_edit()?,
            7 => super::session::cmd_session_wizard(&SessionListArgs::default())?,
            8 => super::auth::login_interactive()?,
            _ => unreachable!(),
        }
    }
}
