//! Locating external programs consistently across shell and desktop launches.
mod discovery;
#[cfg(unix)]
mod unix;

pub use discovery::{bun_program, omp_program, program};
#[cfg(unix)]
pub use unix::{extend_path_for_desktop_launch, find_program};
