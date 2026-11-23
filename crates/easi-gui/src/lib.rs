//! easi-gui: Slint view over the easi-core store. UI binds to state; callbacks only dispatch.

// Slint-generated code uses unwrap internally; our own code stays under the workspace lints.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod ui {
    slint::include_modules!();
}

use slint::ComponentHandle;

pub fn run() -> Result<(), slint::PlatformError> {
    ui::AppWindow::new()?.run()
}
