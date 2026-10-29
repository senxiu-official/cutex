//! No home/config/path CLI. Exactly one bounded stdin envelope -> redacted JSON.
use std::io::{Read, Write};

fn main() {
    use cutex::upgrade::kit_reader;
    std::panic::set_hook(Box::new(|_| {})); // underlying panic prose is not evidence
    let result = if std::env::args_os().len() != 1 {
        kit_reader::boundary_refusal("arguments_not_supported")
    } else {
        let mut bytes = Vec::new();
        match std::io::stdin()
            .lock()
            .take((kit_reader::MAX_INPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
        {
            Ok(_) => std::panic::catch_unwind(|| kit_reader::validate_stdin(&bytes))
                .unwrap_or_else(|_| kit_reader::boundary_refusal("parser_panicked")),
            Err(_) => kit_reader::boundary_refusal("stdin_unavailable"),
        }
    };
    let ok = result["ok"] == true;
    let mut stdout = std::io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &result).is_err() || stdout.write_all(b"\n").is_err() {
        std::process::exit(3);
    }
    std::process::exit(if ok { 0 } else { 2 });
}
