//! Launch command data structure and small builder helpers.

use std::process::Command;

use crate::platform::command::shell_quote;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchCommand {
    pub program: String,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub env_removes: Vec<String>,
}

impl LaunchCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
            env_removes: Vec::new(),
        }
    }

    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }

    pub fn args<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(values.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env_removes.push(key.into());
        self
    }

    /// Removes both an inherited value and any value added earlier to this launch plan.
    pub fn env_unset(mut self, key: impl Into<String>) -> Self {
        let key = key.into();
        self.envs.retain(|(candidate, _)| candidate != &key);
        if !self.env_removes.iter().any(|candidate| candidate == &key) {
            self.env_removes.push(key);
        }
        self
    }

    pub fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        for key in &self.env_removes {
            cmd.env_remove(key);
        }
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        cmd
    }

    pub fn to_shell_command(&self) -> String {
        let mut parts = Vec::new();
        if !self.env_removes.is_empty() {
            parts.push("env".to_string());
            for key in &self.env_removes {
                parts.push("-u".to_string());
                parts.push(shell_quote(key));
            }
        }
        for (key, value) in &self.envs {
            parts.push(format!("{key}={}", shell_quote(value)));
        }
        parts.push(shell_quote(&self.program));
        parts.extend(self.args.iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_unset_removes_planned_values_and_deduplicates_inherited_removal() {
        let launch = LaunchCommand::new("program")
            .env("CUTEX_OBSERVER_URL", "http://127.0.0.1")
            .env_remove("CUTEX_OBSERVER_URL")
            .env_unset("CUTEX_OBSERVER_URL");

        assert!(launch
            .envs
            .iter()
            .all(|(key, _)| key != "CUTEX_OBSERVER_URL"));
        assert_eq!(
            launch
                .env_removes
                .iter()
                .filter(|key| key.as_str() == "CUTEX_OBSERVER_URL")
                .count(),
            1
        );
    }
}
