//! Codex compatibility shims.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;

use crate::config::paths::runtime_dir;
use crate::config::text::write_optional_text_if_changed;
use crate::launch::program::program_name;
use crate::platform::command::shell_quote;
use crate::profiles::model::RuntimeConfig;

pub fn codex_compat_install_dir_for_host_launch(
    runtime: &RuntimeConfig,
    program: &str,
) -> anyhow::Result<Option<PathBuf>> {
    if !matches!(runtime, RuntimeConfig::Host) {
        return Ok(None);
    }

    if program_name(program) == "codex" {
        return Ok(None);
    }

    Ok(Some(ensure_codex_compat_install_dir(program)?))
}

fn ensure_codex_compat_install_dir(program: &str) -> anyhow::Result<PathBuf> {
    let install_dir = runtime_dir()?.join("bin");
    fs::create_dir_all(&install_dir).with_context(|| {
        format!(
            "Failed to create Codex compatibility bin dir: {}",
            install_dir.display()
        )
    })?;

    let target = resolve_program_for_wrapper(program);
    let wrapper = install_dir.join("codex");
    let contents = format!("#!/usr/bin/env sh\nexec {} \"$@\"\n", shell_quote(&target));
    write_optional_text_if_changed(&wrapper, Some(&contents))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
    }

    Ok(install_dir)
}

fn resolve_program_for_wrapper(program: &str) -> String {
    let path = Path::new(program);
    if path.is_absolute() || program.contains('/') || program.contains('\\') {
        return program.to_string();
    }

    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        })
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| program.to_string())
}
