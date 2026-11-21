//! "What's new" (the empty conversation pane, Settings > About). The notes themselves live on the
//! GitHub releases page: embedding them took a notes chain across releases that one release made
//! without the script would break.

use super::*;

/// every release newest first, so an update that skipped versions loses nothing; the anchor
/// skips releases newer than the running one, not installed yet
const RELEASES: &str = concat!(
    "https://github.com/Othereum/sigwin/releases#release-v",
    env!("CARGO_PKG_VERSION")
);

pub(crate) fn wire(app: &AppWindow) {
    app.on_open_whatsnew(|| link::open(RELEASES));
}
