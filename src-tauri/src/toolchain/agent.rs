//! Which coding agent OMBP drives: upstream `omp` or the `omsp` fork, chosen per process by `OMBP_AGENT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Omp,
    Omsp,
}

impl Agent {
    pub fn parse(value: Option<&str>) -> Result<Agent, String> {
        match value.unwrap_or("") {
            "" | "omp" => Ok(Agent::Omp),
            "omsp" => Ok(Agent::Omsp),
            other => Err(format!("OMBP_AGENT must be omp or omsp, not {other:?}")),
        }
    }

    /// main() rejects invalid values at startup, so the fallback only applies in tests.
    pub fn current() -> Agent {
        static AGENT: std::sync::LazyLock<Agent> = std::sync::LazyLock::new(|| {
            Agent::parse(std::env::var("OMBP_AGENT").ok().as_deref()).unwrap_or(Agent::Omp)
        });
        *AGENT
    }

    pub fn command(self) -> &'static str {
        match self {
            Agent::Omp => "omp",
            Agent::Omsp => "omsp",
        }
    }

    pub fn sdk_package(self) -> &'static str {
        match self {
            Agent::Omp => "@oh-my-pi/pi-coding-agent",
            Agent::Omsp => "@oh-my-striped-pi/pi-coding-agent",
        }
    }

    /// Default home config dir and per-repository config dir.
    pub fn dir_name(self) -> &'static str {
        match self {
            Agent::Omp => ".omp",
            Agent::Omsp => ".omsp",
        }
    }

    pub fn config_dir_variable(self) -> &'static str {
        match self {
            Agent::Omp => "PI_CONFIG_DIR",
            Agent::Omsp => "OMSP_CONFIG_DIR",
        }
    }

    pub fn agent_dir_variable(self) -> &'static str {
        match self {
            Agent::Omp => "PI_CODING_AGENT_DIR",
            Agent::Omsp => "OMSP_CODING_AGENT_DIR",
        }
    }

    pub fn profile_variables(self) -> &'static [&'static str] {
        match self {
            Agent::Omp => &["OMP_PROFILE", "PI_PROFILE"],
            Agent::Omsp => &["OMSP_PROFILE"],
        }
    }

    pub fn manifest_keys(self) -> &'static [&'static str] {
        match self {
            Agent::Omp => &["omp", "pi"],
            Agent::Omsp => &["omsp", "omp", "pi"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Agent;

    #[test]
    fn parse_accepts_only_known_agents() {
        assert_eq!(Agent::parse(None), Ok(Agent::Omp));
        assert_eq!(Agent::parse(Some("")), Ok(Agent::Omp));
        assert_eq!(Agent::parse(Some("omp")), Ok(Agent::Omp));
        assert_eq!(Agent::parse(Some("omsp")), Ok(Agent::Omsp));
        assert!(Agent::parse(Some("x")).is_err());
    }
}
