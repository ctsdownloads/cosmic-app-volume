// SPDX-License-Identifier: GPL-3.0-only

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt::init();
    let _ = tracing_log::LogTracer::init();

    tracing::info!("Starting app-volume applet with version {VERSION}");

    cosmic_ext_applet_app_volume::run()
}
