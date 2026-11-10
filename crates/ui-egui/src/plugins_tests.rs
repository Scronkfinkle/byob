//! Headless tests of the plugin card, the Plugins panel and the first-open card: what they
//! draw and what clicking them sends.

use byob_protocol::{PluginLoadError, PluginTest, ServerInfo, Step};
use egui::{Event, Pos2, Rect, Shape};

use super::*;

pub(crate) fn info(id: &str, scope: PluginScope, origin: PluginOrigin) -> PluginInfo {
    PluginInfo {
        id: id.into(),
        name: id.into(),
        version: "0.1.0".into(),
        description: format!("The {id} plugin"),
        icon: String::new(),
        scope,
        origin,
        dir: format!("/d/{id}"),
        enabled: true,
        enabled_from: Level::Default,
        trusted: origin != PluginOrigin::Repo,
        declined: false,
        shadowed_by: None,
        tools: vec![],
        skills: vec![],
        agents: vec![],
        commands: vec![],
        servers: vec![],
        settings: vec![],
        network: false,
        problems: vec![],
        last_error: None,
        fingerprint: "0".into(),
    }
}

fn weather(set: bool) -> PluginInfo {
    let mut p = info(SCRATCH, PluginScope::Chat, PluginOrigin::Agent);
    p.tools = vec!["get_weather".into()];
    p.settings = vec![PluginSetting { key: "API_KEY".into(), label: "API key".into(), help: None, secret: true, required: true, set }];
    p
}

pub(crate) fn ext(plugins: Vec<PluginInfo>) -> ServerMsg {
    ServerMsg::Extensions(Box::new(Extensions { chat: Some("c".into()), workspace: Some("w".into()), plugins, errors: vec![], slash: vec![] }))
}

fn card() -> PluginCard {
    PluginCard {
        plugin: SCRATCH.into(),
        scope: PluginScope::Chat,
        action: PluginAction::Made,
        tools: vec!["get_weather".into()],
        added: vec!["get_weather".into()],
        removed: vec![],
        test: Some(PluginTest { ok: false, output: "KeyError: API_KEY".into() }),
        problems: vec![],
        missing_settings: vec!["API key".into()],
    }
}

/// Texts drawn in one frame, with where.
pub(crate) type Texts = Vec<(String, Rect)>;

fn collect(s: &Shape, out: &mut Texts) {
    match s {
        Shape::Text(t) => out.push((t.galley.text().to_owned(), t.visual_bounding_rect())),
        Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
        _ => {}
    }
}

/// Every text in a frame's output.
pub(crate) fn texts_of(out: &egui::FullOutput) -> Texts {
    let mut texts = Vec::new();
    for s in &out.shapes {
        collect(&s.shape, &mut texts);
    }
    texts
}

fn frame(ctx: &egui::Context, events: Vec<Event>, draw: &mut dyn FnMut(&mut egui::Ui)) -> Texts {
    let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(700.0, 900.0))), events, ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| draw(ui));
    out.textures_delta.clear();
    texts_of(&out)
}

fn click_at(ctx: &egui::Context, at: Pos2, draw: &mut dyn FnMut(&mut egui::Ui)) -> Texts {
    let press = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    frame(ctx, vec![Event::PointerMoved(at), press(true)], draw);
    frame(ctx, vec![press(false)], draw)
}

fn find(texts: &Texts, label: &str) -> Rect {
    texts.iter().find(|(t, _)| t == label).unwrap_or_else(|| panic!("no {label:?} in {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>())).1
}

fn click(ctx: &egui::Context, texts: &Texts, label: &str, draw: &mut dyn FnMut(&mut egui::Ui)) -> Texts {
    click_at(ctx, find(texts, label).center(), draw)
}

pub(crate) fn has(texts: &Texts, s: &str) -> bool {
    texts.iter().any(|(t, _)| t.contains(s))
}

#[test]
fn the_card_takes_a_secret_once_and_never_shows_it() {
    let ctx = egui::Context::default();
    let mut p = PluginsUi::default();
    p.apply(&ext(vec![weather(false)]));
    let c = card();
    let t = Tokens::dark();
    frame(&ctx, vec![], &mut |ui| p.card(ui, &t, 9, &c, true));
    let texts = frame(&ctx, vec![], &mut |ui| p.card(ui, &t, 9, &c, true));
    assert!(has(&texts, "Test run failed") && has(&texts, "KeyError"), "a failed test shows its output");
    assert!(has(&texts, "+get_weather") && has(&texts, "API key"));
    // Type the key into the (password) field and press Enter.
    click(&ctx, &texts, "secret value", &mut |ui| p.card(ui, &t, 9, &c, true));
    let typed = frame(&ctx, vec![Event::Text("sk-very-secret".into())], &mut |ui| p.card(ui, &t, 9, &c, true));
    assert!(!has(&typed, "sk-very-secret"), "a secret field shows dots");
    let enter = Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
    frame(&ctx, vec![enter], &mut |ui| p.card(ui, &t, 9, &c, true));
    assert_eq!(p.take(), [ClientMsg::SetPluginSetting { plugin: SCRATCH.into(), key: "API_KEY".into(), value: Some("sk-very-secret".into()) }]);
    assert!(p.drafts.is_empty(), "the value is forgotten once sent");
    // The engine says it is set: "set ✓", never the value.
    p.apply(&ext(vec![weather(true)]));
    frame(&ctx, vec![], &mut |ui| p.card(ui, &t, 9, &c, true));
    let after = frame(&ctx, vec![], &mut |ui| p.card(ui, &t, 9, &c, true));
    assert!(has(&after, "set ✓") && !has(&after, "sk-very-secret"));
    assert!(after.iter().all(|(_, r)| r.max.y < 300.0), "the card stays compact: {after:?}");
    // Keep it: the scratch plugin is kept under its tool's name.
    click(&ctx, &after, "Keep everywhere", &mut |ui| p.card(ui, &t, 9, &c, true));
    assert_eq!(p.take(), [ClientMsg::KeepPlugin { plugin: SCRATCH.into(), scope: PluginScope::Global, new_id: Some("get-weather".into()) }]);
    click(&ctx, &after, "Keep in this workspace", &mut |ui| p.card(ui, &t, 9, &c, true));
    assert_eq!(p.take(), [ClientMsg::KeepPlugin { plugin: SCRATCH.into(), scope: PluginScope::Workspace, new_id: Some("get-weather".into()) }]);
    // Files: the panel, on this plugin.
    click(&ctx, &after, "Files", &mut |ui| p.card(ui, &t, 9, &c, true));
    assert!(p.take_open_request() && p.focus.as_deref() == Some(SCRATCH) && p.reveal);
    // An earlier card of the same plugin is only a record: no fields, no Keep.
    let old = frame(&ctx, vec![], &mut |ui| p.card(ui, &t, 8, &c, false));
    assert!(has(&old, "Test run failed") && !has(&old, "Keep everywhere") && !has(&old, "API key"));
    // Light theme, narrow: still draws.
    let light = Tokens::light();
    let narrow = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(320.0, 700.0))), ..Default::default() };
    let mut out = ctx.run_ui(narrow, |ui| p.card(ui, &light, 9, &c, true));
    out.textures_delta.clear();
}

#[test]
fn the_panel_switches_resets_removes_and_shows_errors() {
    let ctx = egui::Context::default();
    ctx.global_style_mut(|s| s.animation_time = 0.0);
    let mut p = PluginsUi::default();
    let mut ws = info("lint", PluginScope::Workspace, PluginOrigin::Agent);
    ws.enabled = false;
    ws.enabled_from = Level::Workspace;
    ws.servers = vec![ServerInfo { name: "srv".into(), state: ServerState::Failed, tools: 0, error: Some("exited".into()) }];
    let global = info("pdf", PluginScope::Global, PluginOrigin::User);
    let mut msg = ext(vec![ws, global]);
    if let ServerMsg::Extensions(e) = &mut msg {
        e.errors = vec![PluginLoadError { scope: PluginScope::Global, path: "/p/broken".into(), message: "bad JSON".into() }];
    }
    p.apply(&msg);
    assert_eq!(p.summary(), Some((1, true)), "one on; a load error is trouble");
    let t = Tokens::dark();
    frame(&ctx, vec![], &mut |ui| p.panel(ui, &t, &Filter::All));
    let texts = frame(&ctx, vec![], &mut |ui| p.panel(ui, &t, &Filter::All));
    assert!(has(&texts, "bad JSON") && has(&texts, "THIS WORKSPACE") && has(&texts, "EVERYWHERE"));
    // Open the workspace plugin: where its switch comes from, reset, its failed server.
    p.focus = Some("lint".into());
    frame(&ctx, vec![], &mut |ui| p.panel(ui, &t, &Filter::All));
    let open = frame(&ctx, vec![], &mut |ui| p.panel(ui, &t, &Filter::All));
    assert!(has(&open, "Off: set in this workspace") && has(&open, "exited"));
    click(&ctx, &open, "Reset", &mut |ui| p.panel(ui, &t, &Filter::All));
    assert_eq!(p.take(), [ClientMsg::SetPluginEnabled { scope: ConfigScope::Workspace { id: "w".into() }, plugin: "lint".into(), enabled: None }]);
    // Remove asks first.
    click(&ctx, &open, "Remove…", &mut |ui| p.panel(ui, &t, &Filter::All));
    let asked = frame(&ctx, vec![], &mut |ui| p.panel(ui, &t, &Filter::All));
    assert!(p.take().is_empty() && has(&asked, "Delete the folder of lint"));
    click(&ctx, &asked, "Remove", &mut |ui| p.panel(ui, &t, &Filter::All));
    assert_eq!(p.take(), [ClientMsg::RemovePlugin { plugin: "lint".into(), scope: PluginScope::Workspace }]);
    // A workspace's own settings list only its plugins (for the chat in view's workspace).
    let only = frame(&ctx, vec![], &mut |ui| p.panel(ui, &t, &Filter::Workspace("w".into())));
    assert!(has(&only, "lint") && !has(&only, "pdf") && !has(&only, "bad JSON") && has(&only, "0 of 1 on"));
    let other = frame(&ctx, vec![], &mut |ui| p.panel(ui, &t, &Filter::Workspace("x".into())));
    assert!(has(&other, "Open a chat in this workspace"));
    // The chat popover's switches act for the chat.
    frame(&ctx, vec![], &mut |ui| p.chat_switches(ui, &t));
    let sw = frame(&ctx, vec![], &mut |ui| p.chat_switches(ui, &t));
    let r = find(&sw, "pdf");
    click_at(&ctx, egui::pos2(r.left() - 10.0, r.center().y), &mut |ui| p.chat_switches(ui, &t));
    assert_eq!(p.take(), [ClientMsg::SetPluginEnabled { scope: ConfigScope::Chat, plugin: "pdf".into(), enabled: Some(false) }]);
}

#[test]
fn the_first_open_card_asks_once_and_not_beside_the_approval_card() {
    let ctx = egui::Context::default();
    let mut p = PluginsUi::default();
    let mut a = info("deploy", PluginScope::Workspace, PluginOrigin::Repo);
    a.servers = vec![ServerInfo { name: "k8s".into(), state: ServerState::Off, tools: 0, error: None }];
    a.network = true;
    let b = info("docs", PluginScope::Workspace, PluginOrigin::Repo);
    p.apply(&ext(vec![a.clone(), b.clone()]));
    let t = Tokens::dark();
    let mut entries: Vec<Entry> = Vec::new();
    frame(&ctx, vec![], &mut |ui| p.trust_card(ui, &t, &entries));
    let texts = frame(&ctx, vec![], &mut |ui| p.trust_card(ui, &t, &entries));
    assert!(has(&texts, "This workspace brings plugins") && has(&texts, "servers: k8s · uses the internet"));
    click(&ctx, &texts, "Enable all", &mut |ui| p.trust_card(ui, &t, &entries));
    let all = vec!["deploy".to_owned(), "docs".to_owned()];
    assert_eq!(p.take(), [ClientMsg::TrustPlugins { workspace: "w".into(), plugins: all, trust: true }]);
    // Choose…: the picked ones on, the rest declined.
    click(&ctx, &texts, "Choose…", &mut |ui| p.trust_card(ui, &t, &entries));
    assert!(p.choosing.is_some());
    p.choosing = Some(BTreeSet::from(["docs".to_owned()]));
    let chooser = frame(&ctx, vec![], &mut |ui| p.trust_card(ui, &t, &entries));
    click(&ctx, &chooser, "Turn on 1 selected", &mut |ui| p.trust_card(ui, &t, &entries));
    assert_eq!(
        p.take(),
        [
            ClientMsg::TrustPlugins { workspace: "w".into(), plugins: vec!["docs".into()], trust: true },
            ClientMsg::TrustPlugins { workspace: "w".into(), plugins: vec!["deploy".into()], trust: false }
        ]
    );
    // While the turn's approval card asks the same, this card stays away.
    let waiting = Step {
        id: 1,
        tool: TRUST_STEP.into(),
        title: "Turn on this workspace's plugins".into(),
        access: byob_protocol::ToolAccess::Execute,
        status: StepStatus::Waiting,
        detail: String::new(),
        changes: vec![],
        at: 0,
        media: vec![],
        preview: None,
        artifacts: vec![],
        delegation: None,
        ask: None,
        plugin: None,
    };
    entries.push(Entry {
        id: 2,
        role: byob_protocol::EntryRole::Assistant,
        content: String::new(),
        reasoning: String::new(),
        done: false,
        error: None,
        stats: None,
        steps: vec![waiting],
        undoable: false,
        undone: false,
        attachments: vec![],
    });
    let hidden = frame(&ctx, vec![], &mut |ui| p.trust_card(ui, &t, &entries));
    assert!(!has(&hidden, "This workspace brings"));
    // Answered (either way): the engine's next list has them decided, and the card is gone.
    entries.clear();
    let mut decided = a;
    decided.trusted = true;
    let mut declined = b;
    declined.declined = true;
    p.apply(&ext(vec![decided, declined]));
    let gone = frame(&ctx, vec![], &mut |ui| p.trust_card(ui, &t, &entries));
    assert!(!has(&gone, "This workspace brings") && p.undecided().is_empty());
}
