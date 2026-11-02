//! Locating external programs consistently across shell and desktop launches.
mod agent;
mod discovery;
#[cfg(unix)]
mod unix;

pub use agent::Agent;
pub use discovery::{bun_program, omp_program};
#[cfg(unix)]
pub use unix::{extend_path_for_desktop_launch, find_program};
