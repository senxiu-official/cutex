//! Native feature pins cutex applies to every cute-codex launch it builds.
//!
//! `instant_interrupt` (owner decision D2, native r53 / upstream 0.162): with
//! the feature on, new user input preempts a streaming model response. That
//! interacts with cutex steer, the queue and ExternalInput `soon` delivery, so
//! cutex pins it off until it is evaluated separately. Natives before r53 know
//! the key with default off (r47 - r52 list it as under development), and an
//! unknown `features.*` key is only a warning in every native, so the pin is
//! safe for agents that stay pinned to an older runtime.

/// `-c` value that keeps native instant interrupts off.
pub const INSTANT_INTERRUPT_OFF_OVERRIDE: &str = "features.instant_interrupt=false";

/// The `-c` pair for [`INSTANT_INTERRUPT_OFF_OVERRIDE`].
pub fn instant_interrupt_off_args() -> [String; 2] {
    ["-c".into(), INSTANT_INTERRUPT_OFF_OVERRIDE.into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_interrupt_override_parses_as_native_toml_false() {
        let value: toml::Value = toml::from_str(&instant_interrupt_off_args()[1]).unwrap();
        assert_eq!(value["features"]["instant_interrupt"].as_bool(), Some(false));
        assert_eq!(instant_interrupt_off_args()[0], "-c");
    }
}
