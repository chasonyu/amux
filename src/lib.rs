//! amux — terminal control plane wrapping coding-agent CLIs in real PTYs.

pub mod appearance;
pub mod config;
pub mod escape;
pub mod lock;
pub mod mouse;
pub mod provider;
pub mod pty;
pub mod raw_input;
pub mod session;
pub mod shell;
pub mod theme;
pub mod workspace;

pub use config::AmuxConfig;
pub use escape::EscapeToggle;
pub use mouse::translate_sgr_mouse_clipped;
pub use raw_input::{RawInputParser, split_sequences};
