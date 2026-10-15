use std::io;
use std::io::Write;

use anyhow::anyhow;
use anyhow::Context;

use cutex::platform::command::shell_quote;
use cutex::ui::format::optional_label;
use cutex::ui::format::optional_u64_label;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";

pub(crate) fn prompt_line(label: &str, default: &str) -> anyhow::Result<String> {
    restore_interactive_console_mode();
    if default.is_empty() {
        print!("{BOLD}{label}{RESET}: ");
    } else {
        print!("{BOLD}{label}{RESET} [{CYAN}{default}{RESET}]: ");
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let input = normalize_prompt_input(&line);
    if input.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(input.to_string())
    }
}

pub(crate) fn prompt_choice(
    label: &str,
    options: &[(&str, &str)],
    default: usize,
) -> anyhow::Result<usize> {
    for (i, (name, desc)) in options.iter().enumerate() {
        let marker = if i + 1 == default {
            format!("{GREEN}▸{RESET}")
        } else {
            format!(" ")
        };
        println!(
            "{marker} {BOLD}[{}]{RESET} {CYAN}{name}{RESET}  {DIM}{desc}{RESET}",
            i + 1
        );
    }
    let answer = prompt_line(label, &default.to_string())?;
    let idx = answer.parse::<usize>().unwrap_or(default);
    if idx < 1 || idx > options.len() {
        Ok(default)
    } else {
        Ok(idx)
    }
}

pub(crate) fn checkbox(value: bool) -> String {
    if value {
        format!("{GREEN}[x]{RESET}")
    } else {
        format!("{DIM}[ ]{RESET}")
    }
}

pub(crate) fn wizard_value(value: impl AsRef<str>) -> String {
    let value = value.as_ref();
    if value.is_empty() || value == "-" {
        format!("{DIM}-{RESET}")
    } else {
        format!("{BOLD}{value}{RESET}")
    }
}

pub(crate) fn parse_optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn cli_args_label(args: &[String]) -> String {
    if args.is_empty() {
        "-".to_string()
    } else {
        args.iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(crate) fn parse_cli_args_value(value: &str) -> anyhow::Result<Vec<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Ok(Vec::new());
    }

    shlex::split(trimmed).ok_or_else(|| anyhow!("Invalid shell-style CLI args: {value}"))
}

pub(crate) fn read_wizard_choice(max: usize) -> anyhow::Result<Option<usize>> {
    restore_interactive_console_mode();
    print!("{CYAN}Select item number{RESET}, or q to quit: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let input = normalize_prompt_input(&line);
    if input.is_empty() || input.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    let choice = input
        .parse::<usize>()
        .with_context(|| format!("Invalid menu selection: {input}"))?;
    if choice == 0 || choice > max {
        anyhow::bail!("Menu selection out of range: {choice}");
    }
    Ok(Some(choice))
}

pub(crate) fn normalize_prompt_input(line: &str) -> &str {
    line.trim().trim_start_matches('\u{feff}').trim()
}

#[cfg(windows)]
fn restore_interactive_console_mode() {
    windows_console_mode::restore_interactive_console_mode();
}

#[cfg(not(windows))]
fn restore_interactive_console_mode() {}

#[cfg(windows)]
mod windows_console_mode {
    use std::ffi::c_void;

    type Dword = u32;
    type Bool = i32;
    type Handle = *mut c_void;

    const FALSE: Bool = 0;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const STD_INPUT_HANDLE: Dword = -10i32 as Dword;
    const STD_OUTPUT_HANDLE: Dword = -11i32 as Dword;
    const ENABLE_PROCESSED_INPUT: Dword = 0x0001;
    const ENABLE_LINE_INPUT: Dword = 0x0002;
    const ENABLE_ECHO_INPUT: Dword = 0x0004;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: Dword = 0x0004;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(std_handle: Dword) -> Handle;
        fn GetConsoleMode(console_handle: Handle, mode: *mut Dword) -> Bool;
        fn SetConsoleMode(console_handle: Handle, mode: Dword) -> Bool;
    }

    pub(super) fn restore_interactive_console_mode() {
        unsafe {
            let stdin = GetStdHandle(STD_INPUT_HANDLE);
            let mut in_mode = 0;
            if !stdin.is_null()
                && stdin != INVALID_HANDLE_VALUE
                && GetConsoleMode(stdin, &mut in_mode) != FALSE
            {
                let interactive =
                    in_mode | ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT;
                if interactive != in_mode {
                    let _ = SetConsoleMode(stdin, interactive);
                }
            }

            let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut out_mode = 0;
            if !stdout.is_null()
                && stdout != INVALID_HANDLE_VALUE
                && GetConsoleMode(stdout, &mut out_mode) != FALSE
            {
                let interactive = out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                if interactive != out_mode {
                    let _ = SetConsoleMode(stdout, interactive);
                }
            }
        }
    }
}

pub(crate) fn prompt_optional_string(
    label: &str,
    current: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let current_label = optional_label(current);
    let value = prompt_line(&format!("{label} (`-` clears)"), &current_label)?;
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

pub(crate) fn prompt_cli_args(label: &str, current: &[String]) -> anyhow::Result<Vec<String>> {
    let current_label = cli_args_label(current);
    let value = prompt_line(&format!("{label} (`-` clears)"), &current_label)?;
    parse_cli_args_value(&value)
}

pub(crate) fn prompt_optional_u64(
    label: &str,
    current: Option<u64>,
) -> anyhow::Result<Option<u64>> {
    let current_label = optional_u64_label(current);
    let value = prompt_line(&format!("{label} (`-` clears)"), &current_label)?;
    parse_optional_u64(&value)
}

pub(crate) fn prompt_optional_csv(
    label: &str,
    current: Option<&[String]>,
) -> anyhow::Result<Option<Vec<String>>> {
    let current_label = current
        .map(|items| items.join(","))
        .unwrap_or_else(|| "-".to_string());
    let value = prompt_line(&format!("{label} (`-` clears)"), &current_label)?;
    Ok(parse_optional_csv(&value))
}

pub(crate) fn parse_optional_u64(value: &str) -> anyhow::Result<Option<u64>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Ok(None);
    }
    let parsed = trimmed
        .parse::<u64>()
        .with_context(|| format!("Unsupported integer value: {value}"))?;
    Ok(Some(parsed))
}

pub(crate) fn parse_optional_csv(value: &str) -> Option<Vec<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return None;
    }
    Some(
        trimmed
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| item.replace('-', "_"))
            .collect(),
    )
}
