// Stderr is diagnostic. A failed service-log write must not unwind a worker
// or abort the process in a destructor. Stdout retains its CLI output semantics.
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), $($arg)*);
    }};
}
