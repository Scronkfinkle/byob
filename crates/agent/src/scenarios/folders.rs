//! A project's linked folders: written to, undone, and closed when swapped for a link.

use serde_json::json;

use super::{Answer, StepCheck, World};

/// A project links a second folder (the user's notes). A request writes there and in the
/// project; undo takes both back. Regression (0b8bf63): undo refused the whole turn, the
/// linked folder's change being "outside the workspace".
///
/// Then the linked folder is moved away and a link to somewhere else put in its place.
/// Regression (7e0c3ce): the sandbox kept trusting the path, so file tools reached through
/// the link. Now that place is refused, and the refusal reaches the model. (Unix: the link
/// is a symlink.)
#[cfg(unix)]
#[test]
fn a_linked_folder_is_undone_with_the_turn_and_closed_once_swapped_for_a_link() {
    let mut w = World::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let elsewhere = elsewhere.path().canonicalize().unwrap();
    let notes = elsewhere.join("notes");
    std::fs::create_dir(&notes).unwrap();
    std::fs::write(notes.join("garden.md"), "# Garden\n").unwrap();
    w.link_folders(&[&notes]);
    let at = |name: &str| notes.join(name).display().to_string();

    w.model.calls("write_file", json!({"path": "plan.md", "content": "Plant beans in May.\n"}));
    w.model.calls("edit_file", json!({"path": at("garden.md"), "edits": [{"find": "# Garden\n", "replace": "# Garden\nBeans: May\n"}]}));
    w.model.calls("write_file", json!({"path": at("seeds.md"), "content": "beans\n"}));
    w.model.says("Planned and noted.");
    let turn = w.send("Plan the beans and note it in my garden notes");
    for s in &turn.reply.steps {
        s.done();
    }
    assert_eq!(std::fs::read_to_string(notes.join("garden.md")).unwrap(), "# Garden\nBeans: May\n");
    assert!(notes.join("seeds.md").is_file() && w.read("plan.md").is_some());
    assert!(turn.reply.undoable, "a turn that only touched the project's folders can be undone");

    // Undo takes back the project's file and the linked folder's alike.
    w.undo(turn.reply.id).expect("undo covers linked folders");
    assert_eq!(std::fs::read_to_string(notes.join("garden.md")).unwrap(), "# Garden\n");
    assert!(!notes.join("seeds.md").exists() && w.read("plan.md").is_none());

    // The notes folder is moved away and a link to a private folder left in its place.
    let private = tempfile::tempdir().unwrap();
    std::fs::write(private.path().join("key"), "s3cret").unwrap();
    std::fs::rename(&notes, elsewhere.join("notes-moved")).unwrap();
    std::os::unix::fs::symlink(private.path(), &notes).unwrap();

    // (Both calls in one answer, as models do.)
    let both = Answer::call("read_file", json!({"path": at("key")})).and("write_file", json!({"path": at("key"), "content": "overwritten"}));
    w.model.answers(move |_| both);
    w.model.answers(|sent| {
        let results = sent.tool_results();
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(results.iter().all(|r| !r.contains("s3cret")), "{results:?}");
        Answer::text("I can't reach that folder.")
    });
    let turn = w.send("Read the key in my notes folder and replace it");
    turn.step("read_file").failed();
    turn.step("write_file").failed();
    assert_eq!(turn.text(), "I can't reach that folder.");
    assert_eq!(std::fs::read_to_string(private.path().join("key")).unwrap(), "s3cret");

    // Back as it was: open again.
    std::fs::remove_file(&notes).unwrap();
    std::fs::rename(elsewhere.join("notes-moved"), &notes).unwrap();
    w.model.calls("read_file", json!({"path": at("garden.md")}));
    w.model.says("Read it.");
    w.send("Read my garden notes").step("read_file").done();
    assert_eq!(w.model.unused(), 0);
    byob_tools::sandbox::set_linked(&w.project, &[]);
}
