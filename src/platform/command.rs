//! Cross-platform command lookup helpers.

use std::path::Path;

pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn command_exists_in_path(command: &str) -> bool {
    if command.trim().is_empty() {
        return false;
    }

    if command.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(command).is_file();
    }

    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };

    #[cfg(windows)]
    {
        let path_has_extension = Path::new(command).extension().is_some();
        let extensions: Vec<String> = if path_has_extension {
            vec![String::new()]
        } else {
            std::env::var_os("PATHEXT")
                .map(|value| {
                    value
                        .to_string_lossy()
                        .split(';')
                        .filter(|part| !part.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .filter(|values: &Vec<String>| !values.is_empty())
                .unwrap_or_else(|| vec![".EXE".to_string(), ".CMD".to_string(), ".BAT".to_string()])
        };

        std::env::split_paths(&path_var).any(|dir| {
            extensions.iter().any(|ext| {
                let candidate = if ext.is_empty() {
                    dir.join(command)
                } else {
                    dir.join(format!("{command}{ext}"))
                };
                candidate.is_file()
            })
        })
    }

    #[cfg(not(windows))]
    {
        std::env::split_paths(&path_var).any(|dir| dir.join(command).is_file())
    }
}
