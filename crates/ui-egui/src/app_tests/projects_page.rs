use super::*;

/// Esc in a dialog over the Projects page closes the dialog and leaves the page: the page
/// used to take the key first and go back to the chat with the dialog still open.
#[test]
fn esc_in_a_dialog_over_the_projects_page_closes_the_dialog_not_the_page() {
    let ctx = egui::Context::default();
    let (_fake, mut app) = two_workspaces();
    frame(&ctx, &mut app, 1000.0);
    app.page = Page::Projects;
    app.workspaces.open_settings("w1", false);
    frame(&ctx, &mut app, 1000.0);
    shortcut(&ctx, &mut app, egui::Key::Escape, egui::Modifiers::NONE);
    assert!(app.workspaces.window.is_none(), "Esc closes the settings");
    assert_eq!(app.page, Page::Projects, "and the page stays");
    app.workspaces.open_start(projects::Start::Choose);
    frame(&ctx, &mut app, 1000.0);
    shortcut(&ctx, &mut app, egui::Key::Escape, egui::Modifiers::NONE);
    assert!(app.workspaces.form.is_none(), "Esc closes Create project");
    assert_eq!(app.page, Page::Projects, "and the page stays");
}

/// Esc in the search clears what's typed (the page stays); a second Esc, in the empty
/// field, leaves it; a third goes back to the chat.
#[test]
fn esc_in_the_search_clears_it_before_leaving_the_page() {
    let ctx = egui::Context::default();
    let (_fake, mut app) = two_workspaces();
    frame(&ctx, &mut app, 1000.0);
    app.page = Page::Projects;
    frame(&ctx, &mut app, 1000.0);
    shortcut(&ctx, &mut app, egui::Key::F, egui::Modifiers::COMMAND);
    frame(&ctx, &mut app, 1000.0);
    assert!(ctx.memory(|m| m.has_focus(egui::Id::unique("projects-search"))), "Ctrl/Cmd F puts the keyboard in the search");
    app.projects.query = "w1".into();
    frame(&ctx, &mut app, 1000.0);
    shortcut(&ctx, &mut app, egui::Key::Escape, egui::Modifiers::NONE);
    assert_eq!((app.page, app.projects.query.as_str()), (Page::Projects, ""), "the first Esc clears the search");
    shortcut(&ctx, &mut app, egui::Key::Escape, egui::Modifiers::NONE);
    assert_eq!(app.page, Page::Projects, "the second leaves the field");
    assert!(!ctx.memory(|m| m.has_focus(egui::Id::unique("projects-search"))));
    shortcut(&ctx, &mut app, egui::Key::Escape, egui::Modifiers::NONE);
    assert_eq!(app.page, Page::Chat, "the third goes back to the chat");
}

/// On the Projects page Ctrl/Cmd K still opens the search palette.
#[test]
fn the_palette_key_still_opens_the_palette_on_the_projects_page() {
    let ctx = egui::Context::default();
    let (_fake, mut app) = two_workspaces();
    frame(&ctx, &mut app, 1000.0);
    app.page = Page::Projects;
    frame(&ctx, &mut app, 1000.0);
    shortcut(&ctx, &mut app, egui::Key::K, egui::Modifiers::COMMAND);
    assert!(app.sidebar.palette.is_some());
}
