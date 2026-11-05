//! The update banner: it appears only when the published release is newer than this build, and it
//! is the only place that offers to install one.

use postino_core::update::UpdateStatus;

use super::*;

fn available(latest: &str, available: bool) -> UpdateStatus {
    UpdateStatus {
        current: "0.1.0".to_owned(),
        latest: latest.to_owned(),
        available,
        url: format!("https://github.com/federico-fabiani/postino/releases/tag/{latest}"),
    }
}

#[test]
fn the_banner_names_the_published_version_and_offers_the_button() {
    let (ctx, mut app) = ready_app();
    app.update = crate::app::update::UpdateUi::seeded(available("v0.1.1", true));
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert!(text.iter().any(|run| run.contains("Postino v0.1.1 è disponibile.")), "the banner did not paint: {text:?}");
    assert!(text.iter().any(|run| run.trim() == "Aggiorna"), "the install button did not paint: {text:?}");
    assert!(text.iter().any(|run| run.trim() == "Più tardi"), "the dismissal did not paint: {text:?}");
    assert_eq!(app.update.offered(), Some("v0.1.1"));
}

#[test]
fn no_banner_when_this_build_is_the_latest_release() {
    let (ctx, mut app) = ready_app();
    app.update = crate::app::update::UpdateUi::seeded(available("v0.1.0", false));
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert!(!text.iter().any(|run| run.contains("è disponibile")), "an up-to-date build showed a banner: {text:?}");
    assert!(!text.iter().any(|run| run.trim() == "Aggiorna"), "an up-to-date build offered an install: {text:?}");
}
