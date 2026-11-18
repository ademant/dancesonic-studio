//! Core of DanceSonic Studio: everything that does not need a user interface.
//!
//! Server clients, the library database, sync, the playback model, the analysis
//! queue and the remote control protocol live here, so they can be tested
//! without GTK or GStreamer.

/// Application ID, used for the GTK application, the desktop file and D-Bus.
pub const APP_ID: &str = "io.github.ademant.DanceSonicStudio";

/// Human-readable application name.
pub const APP_NAME: &str = "DanceSonic Studio";

#[cfg(test)]
mod tests {
    use super::*;

    /// GTK and D-Bus require a reverse-DNS ID with at least three elements,
    /// each starting with a letter and containing only `[A-Za-z0-9_]`.
    #[test]
    fn app_id_is_a_valid_dbus_name() {
        let parts: Vec<&str> = APP_ID.split('.').collect();
        assert!(parts.len() >= 3);
        for part in parts {
            let mut chars = part.chars();
            assert!(
                chars.next().is_some_and(|c| c.is_ascii_alphabetic()),
                "{part}"
            );
            assert!(
                chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{part}"
            );
        }
    }
}
