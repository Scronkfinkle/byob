//! Images for models that may not see them.

use super::*;

/// A 1×1 PNG.
const PIXEL: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06,
    0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x60, 0xf8, 0x5f, 0x0f, 0x00, 0x02, 0x87,
    0x01, 0x80, 0xeb, 0x47, 0xba, 0x92, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// Regression: with a server whose vision byob couldn't tell (most besides llama-server),
/// an image went to a model that can't take images, the server refused the request and the
/// reply failed. The request goes again without the images, and the user is told.
#[test]
fn a_model_that_refuses_images_gets_the_request_without_them() {
    let mut w = World::new();
    std::fs::write(w.project.join("chart.png"), PIXEL).unwrap();
    w.model.calls("look_at_image", json!({"path": "chart.png"}));
    w.model.answers(|r| {
        assert!(r.0.to_string().contains("data:image/"), "the image was sent");
        Answer::http(400, "image input is not supported - hint: if this is unexpected, you may need to provide the mmproj")
    });
    w.model.answers(|r| {
        assert!(!r.0.to_string().contains("data:image/"), "the image was sent again");
        Answer::text("I can't see images, sorry.")
    });
    let turn = w.send("what's in chart.png?");
    assert_eq!(turn.error, None);
    assert_eq!(turn.text(), "I can't see images, sorry.");
    assert!(w.seen.notices.iter().any(|(_, t)| t.contains("can't take images")), "{:?}", w.seen.notices);
}
