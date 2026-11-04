//! The label registry, the label menu and the inline label editor.

use super::*;

#[test]
fn sidebar_shows_the_label_registry_and_filters_the_list() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    let labels = app.labels_cache.clone();
    assert_eq!(labels.len(), 3);
    assert!(labels.iter().any(|label| label.name == "Lavoro" && label.message_count == 6 && label.unread_count == 2));

    app.select_label("lbl_1".to_owned());
    let (rows, total, unread) = app.visible_messages();
    assert_eq!(total, 6, "a label view spans every folder by default");
    assert_eq!(unread, 2);
    assert!(rows.iter().all(|message| message.labels.contains(&"lbl_1".to_owned())));
    app.open_message(rows[0].id.clone());
    frame(&ctx, &mut app, vec![]);
    assert!(app.reading_open, "the labelled message opens with its chips");

    app.select_folder("inbox");
    assert_eq!(app.label_filter.as_deref(), Some("lbl_1"), "a folder click narrows instead of dropping the label");
    assert_eq!(app.label_folder.as_deref(), Some("inbox"));
    assert_eq!(app.visible_messages().1, 2, "the intersection with Posta in arrivo is two messages");

    app.clear_label_filter();
    assert!(app.visible_messages().1 >= 4, "Mostra tutte returns to the folder view");
}

#[test]
fn inline_label_create_and_duplicate_errors_keep_the_field_open() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.push_toast("Messaggio archiviato", UndoAction::Move(Vec::new()));
    assert!(app.toast.is_some(), "the stale toast must exist before the create");
    app.start_label_create();
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Ufficio".to_owned();
        editor.color = LabelColor::Forest;
    }
    app.commit_label_editor();
    assert!(app.label_editor.is_none());
    assert!(app.engine.labels().iter().any(|label| label.name == "Ufficio" && label.color == LabelColor::Forest));
    assert!(app.status.contains("creata"));
    assert!(app.toast.is_none(), "creating a label is not undoable and clears a stale toast");

    app.start_label_create();
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "lavoro".to_owned();
    }
    app.commit_label_editor();
    let editor = app.label_editor.as_ref().expect("duplicate keeps the field open");
    assert_eq!(editor.error.as_deref(), Some("Esiste già un'etichetta con questo nome."));
    assert!(editor.focus, "focus returns to the field after the error");
    frame(&ctx, &mut app, vec![]);

    app.start_label_rename("lbl_1");
    assert_eq!(app.label_editor.as_ref().map(|editor| editor.text.as_str()), Some("Lavoro"));
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Lavoro clienti".to_owned();
        editor.color = LabelColor::Danger;
    }
    app.commit_label_editor();
    assert_eq!(app.engine.label("lbl_1").map(|label| label.name.as_str()), Some("Lavoro clienti"));
    assert_eq!(app.engine.label("lbl_1").map(|label| label.color), Some(LabelColor::Danger));
    assert!(matches!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()), Some(UndoAction::LabelRename { .. })), "rename offers an undo");
    app.undo_last();
    assert_eq!(app.engine.label("lbl_1").map(|label| label.name.as_str()), Some("Lavoro"), "undo restores the previous name");
    assert_eq!(app.engine.label("lbl_1").map(|label| label.color), Some(LabelColor::Gold), "undo restores the previous colour");
}

#[test]
fn label_menu_applies_to_a_multi_selection_as_one_undo_step() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    let (rows, _, _) = app.visible_messages();
    let ids = vec![rows[0].id.clone(), rows[1].id.clone()];
    app.select_only(rows[0].id.clone());
    app.toggle_selection(rows[1].id.clone());
    frame(&ctx, &mut app, vec![key(Key::L, Modifiers::NONE)]);
    assert_eq!(app.label_menu.as_ref().map(|menu| menu.context.target.len()), Some(2), "L opens the menu for the whole selection");
    app.open_label_menu(ids.clone());
    frame(&ctx, &mut app, vec![]);
    app.toggle_label_target("lbl_2");
    assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.labels.contains(&"lbl_2".to_owned()))));
    app.undo_last();
    assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| !message.summary.labels.contains(&"lbl_2".to_owned()))));
}

#[test]
fn deleting_a_label_is_confirmed_and_undoable() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.start_label_create();
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Temporanea".to_owned();
    }
    app.commit_label_editor();
    let id = app.engine.labels().iter().find(|label| label.name == "Temporanea").expect("created").id.clone();
    app.apply_label(&["msg_001".to_owned()], &id, true);
    app.request_label_delete(&id);
    let state = app.label_delete.clone().expect("confirmation dialog");
    assert_eq!(state.count, 1);
    frame(&ctx, &mut app, vec![]);
    app.delete_label(&state);
    assert!(app.engine.label(&id).is_none());
    assert!(app.engine.message("msg_001").is_some_and(|message| !message.summary.labels.contains(&id)));
    app.undo_last();
    let restored = app.engine.labels().iter().find(|label| label.name == "Temporanea").expect("restored label");
    assert_ne!(restored.id, id, "a delete undo recreates the label with a fresh id, never the retired one");
    assert!(app.engine.message("msg_001").is_some_and(|message| message.summary.labels.contains(&restored.id)));
}

#[test]
fn delete_dialog_confirms_with_enter_and_cancels_with_escape() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);

    app.request_label_delete("lbl_2");
    frame(&ctx, &mut app, vec![]);
    frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(app.label_delete.is_none(), "Enter closes the dialog");
    assert!(app.engine.label("lbl_2").is_none(), "Enter confirms the delete");

    app.request_label_delete("lbl_3");
    frame(&ctx, &mut app, vec![]);
    frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(app.label_delete.is_none(), "Esc closes the dialog");
    assert!(app.engine.label("lbl_3").is_some(), "Esc cancels without deleting");
}

#[test]
fn label_menu_removal_from_a_multi_selection_is_one_undo_step() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    let (rows, _, _) = app.visible_messages();
    let ids = vec![rows[0].id.clone(), rows[1].id.clone()];
    app.open_label_menu(ids.clone());
    app.toggle_label_target("lbl_2");
    assert!(ids.iter().all(|id| has_label(&app, id, "lbl_2")), "the first click applies the label");
    app.toggle_label_target("lbl_2");
    assert!(ids.iter().all(|id| !has_label(&app, id, "lbl_2")), "the second click removes it");
    app.undo_last();
    assert!(ids.iter().all(|id| has_label(&app, id, "lbl_2")), "one undo restores the whole batch");
}

#[test]
fn label_menu_targets_are_captured_and_stale_selection_closes_it() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    let (rows, _, _) = app.visible_messages();
    app.select_only(rows[0].id.clone());
    app.open_label_menu(app.action_ids());
    assert_eq!(app.label_menu.as_ref().map(|menu| menu.context.target.clone()), Some(vec![rows[0].id.clone()]));

    app.select_only(rows[1].id.clone());
    assert!(app.label_menu_is_stale(), "the menu notices the selection changed");
    frame(&ctx, &mut app, vec![]);
    assert!(app.label_menu.is_none(), "a stale menu closes instead of labelling the wrong message");
}

#[test]
fn label_menu_row_click_toggles_the_label_through_the_widget() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    let target = vec![app.visible_messages().0[0].id.clone()];
    app.open_label_menu(target.clone());
    // Two frames: the modal area settles its anchored position on the first one, and egui
    // resolves a click against the previous frame's widget rects.
    frame(&ctx, &mut app, vec![]);
    frame(&ctx, &mut app, vec![]);
    let row = probe::menu_rows(&ctx).get("lbl_2").copied().expect("the Personale row");
    click(&ctx, &mut app, row.center());
    assert!(
        has_label(&app, &target[0], "lbl_2"),
        "clicking {row:?} applies the label (status: {}, menu open: {})",
        app.status,
        app.label_menu.is_some()
    );
    frame(&ctx, &mut app, vec![]);
    let row = probe::menu_rows(&ctx).get("lbl_2").copied().expect("the menu stays open after a toggle");
    click(&ctx, &mut app, row.center());
    assert!(!has_label(&app, &target[0], "lbl_2"), "a second click removes it");
}

#[test]
fn label_menu_closes_on_escape_and_on_an_outside_click() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    let target = vec![app.visible_messages().0[0].id.clone()];

    app.open_label_menu(target.clone());
    frame(&ctx, &mut app, vec![]);
    assert!(app.label_menu.is_some());
    frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(app.label_menu.is_none(), "Escape closes the menu");

    app.open_label_menu(target);
    frame(&ctx, &mut app, vec![]);
    assert!(app.label_menu.is_some());
    click(&ctx, &mut app, egui::pos2(900.0, 700.0));
    assert!(app.label_menu.is_none(), "a click outside the menu closes it");
}

#[test]
fn creating_from_the_label_menu_applies_to_the_captured_targets() {
    let mut app = PostinoApp::demo(true);
    let (rows, _, _) = app.visible_messages();
    let targets = vec![rows[0].id.clone(), rows[1].id.clone()];
    app.open_label_menu(targets.clone());
    app.start_label_create_for(targets.clone());
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Cliente".to_owned();
        editor.color = LabelColor::Forest;
    }
    app.commit_label_editor();
    let created = app.engine.labels().iter().find(|label| label.name == "Cliente").expect("created label");
    assert_eq!(created.color, LabelColor::Forest);
    let created_id = created.id.clone();
    assert!(targets.iter().all(|id| has_label(&app, id, &created_id)), "the new label lands on the captured messages");
    assert!(matches!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()), Some(UndoAction::Labels(_))), "the apply is one undo step");
    app.undo_last();
    assert!(targets.iter().all(|id| !has_label(&app, id, &created_id)));
    assert!(app.engine.labels().iter().any(|label| label.name == "Cliente"), "undo removes the assignment, not the label");
}

#[test]
fn label_menu_renders_with_an_empty_registry() {
    let (ctx, mut app) = ready_app();
    for id in ["lbl_1", "lbl_2", "lbl_3"] {
        app.engine.call("label_delete", json!({ "id": id })).expect("delete seeded label");
    }
    app.refresh_labels();
    assert!(app.labels_cache.is_empty());
    let (rows, _, _) = app.visible_messages();
    app.open_label_menu(vec![rows[0].id.clone()]);
    frame(&ctx, &mut app, vec![]);
    assert!(app.label_menu.is_some(), "the menu stays open to offer creation");
}

#[test]
fn f2_renames_the_active_label() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.select_label("lbl_1".to_owned());
    frame(&ctx, &mut app, vec![]);
    frame(&ctx, &mut app, vec![key(Key::F2, Modifiers::NONE)]);
    assert!(
        matches!(app.label_editor.as_ref().map(|editor| &editor.mode), Some(LabelEditorMode::Rename(id)) if id == "lbl_1"),
        "F2 opens the rename editor on the active label"
    );
    frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(app.label_editor.is_none());
}

#[test]
fn shortcuts_survive_a_focused_label_row() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.select_label("lbl_1".to_owned());
    frame(&ctx, &mut app, vec![]);
    let row = probe::rows(&ctx).get("Lavoro").copied().expect("the Lavoro row is on screen");
    click(&ctx, &mut app, row.center());
    assert!(app.label_editor.is_none());
    assert!(ctx.memory(|memory| memory.focused()).is_some(), "the clicked row must hold keyboard focus for this test to mean anything");
    frame(&ctx, &mut app, vec![key(Key::F2, Modifiers::NONE)]);
    assert!(
        matches!(app.label_editor.as_ref().map(|editor| &editor.mode), Some(LabelEditorMode::Rename(id)) if id == "lbl_1"),
        "F2 must still work while a non-text widget has focus"
    );
}

#[test]
fn a_focused_text_field_still_suppresses_the_single_key_map() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.select_label("lbl_1".to_owned());
    frame(&ctx, &mut app, vec![]);
    frame(&ctx, &mut app, vec![key(Key::Slash, Modifiers::NONE)]);
    frame(&ctx, &mut app, vec![]);
    assert!(ctx.text_edit_focused(), "the search field owns the keyboard");
    frame(&ctx, &mut app, vec![key(Key::F2, Modifiers::NONE)]);
    assert!(app.label_editor.is_none(), "a focused text field must still swallow F2");
}

#[test]
fn plus_while_editing_keeps_the_draft() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.start_label_create();
    frame(&ctx, &mut app, vec![]);
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Ufficio".to_owned();
    }
    app.start_label_create_or_focus();
    let editor = app.label_editor.as_ref().expect("the editor stays open");
    assert_eq!(editor.text, "Ufficio", "pressing + must not discard the draft");
    assert!(editor.focus, "it returns focus to the field");
}

#[test]
fn editor_pointer_controls_save_and_cancel() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.start_label_create();
    frame(&ctx, &mut app, vec![]);
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Da mouse".to_owned();
    }
    frame(&ctx, &mut app, vec![]);
    let save = probe::saved_at(&ctx).expect("the Salva control is on screen");
    click(&ctx, &mut app, save.center());
    assert!(app.label_editor.is_none(), "Salva commits the field");
    assert!(app.labels_cache.iter().any(|label| label.name == "Da mouse"));

    app.start_label_create();
    frame(&ctx, &mut app, vec![]);
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Scartata".to_owned();
    }
    frame(&ctx, &mut app, vec![]);
    let cancel = probe::cancel_at(&ctx).expect("the Annulla control is on screen");
    click(&ctx, &mut app, cancel.center());
    assert!(app.label_editor.is_none(), "Annulla closes the editor");
    assert!(!app.labels_cache.iter().any(|label| label.name == "Scartata"));
}

#[test]
fn inline_editor_enter_commits_and_escape_cancels() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.start_label_create();
    frame(&ctx, &mut app, vec![]);
    assert!(app.label_editor.as_ref().is_some_and(|editor| editor.focused), "the field owns the keyboard");
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Da tastiera".to_owned();
    }
    frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(app.label_editor.is_none(), "Enter saves");
    assert!(app.labels_cache.iter().any(|label| label.name == "Da tastiera"));

    app.start_label_create();
    frame(&ctx, &mut app, vec![]);
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Scartata".to_owned();
    }
    frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(app.label_editor.is_none(), "Esc cancels");
    assert!(!app.labels_cache.iter().any(|label| label.name == "Scartata"));
}

#[test]
fn deleting_a_label_with_more_than_five_hundred_assignments_restores_them_all() {
    let mut app = PostinoApp::demo(true);
    let mut ids = Vec::new();
    for index in 0..501 {
        let value = app.engine.call("draft_save", json!({ "subject": format!("Bozza bulk {index}"), "body_text": "x" })).expect("draft saved");
        let saved = serde_json::from_value::<DraftSave>(value).expect("draft response");
        ids.push(saved.draft.summary.id);
    }
    let value = app.engine.call("label_create", json!({ "name": "Bulk", "color": "muted" })).expect("label created");
    let label = serde_json::from_value::<LabelCreate>(value).expect("label response").label;
    for chunk in ids.chunks(LABEL_APPLY_CHUNK) {
        app.engine.call("label_apply", json!({ "id": label.id, "message_ids": chunk, "apply": true })).expect("label applied");
    }
    app.refresh_labels();
    app.request_label_delete(&label.id);
    let state = app.label_delete.clone().expect("confirmation dialog");
    assert_eq!(state.count, 501);
    app.delete_label(&state);
    assert!(app.engine.label(&label.id).is_none());
    app.undo_last();
    let restored = app.engine.labels().iter().find(|candidate| candidate.name == "Bulk").expect("label restored");
    assert_ne!(restored.id, label.id, "the recreated label gets a new id");
    assert!(ids.iter().all(|id| has_label(&app, id, &restored.id)), "undo restores all 501 assignments through the 500-id chunk cap");
    assert!(ids.iter().all(|id| !has_label(&app, id, &label.id)), "the retired id carries no assignment");
    assert_eq!(app.labels_cache.iter().find(|candidate| candidate.id == restored.id).map(|candidate| candidate.message_count), Some(501));
}
