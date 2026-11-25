//! The generated UI (`ui/app.slint`). Re-exported from the main crate's root. Its own crate so a
//! Rust-only edit doesn't reparse it (6.7s -> 3.5s in the day-to-day build). Anything Rust uses must be
//! `export`ed from `app.slint`.

slint::include_modules!();
