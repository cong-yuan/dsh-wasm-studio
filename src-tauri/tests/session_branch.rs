use std::path::PathBuf;

use dsh_wasm_studio_lib::studio::Studio;
use serde_json::json;

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wasm-studio-session-branch-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn fork_at_assistant_turn_keeps_completed_prefix_and_survives_resume() {
    let dir = tmpdir("assistant");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .create_agent(
            Some("source".into()),
            "mock".into(),
            "mock-1".into(),
            Some("/tmp".into()),
        )
        .unwrap();
    studio
        .send_message("source", "one".into(), "u1".into())
        .await
        .unwrap();
    studio
        .send_message("source", "two".into(), "u2".into())
        .await
        .unwrap();

    // Simple mock transcript: 0=user(one), 1=assistant(one), 2=user(two), 3=assistant(two).
    let result = studio
        .fork_session(
            "source",
            json!({ "role": "assistant", "entryId": "studio-entry:1:assistant" }),
        )
        .await
        .unwrap();
    let child = result["sessionId"].as_str().unwrap().to_string();

    let transcript = studio.transcript(&child).unwrap();
    assert_eq!(transcript.len(), 2);
    assert_eq!(transcript[0].role, "user");
    assert_eq!(transcript[0].text, "one");
    assert_eq!(transcript[1].role, "assistant");
    assert_eq!(transcript[1].text, "one");

    // The inherited prefix must be durable, not only present in the in-memory child.
    studio.soft_unbind_agent(&child).unwrap();
    studio.resume_session(&child).unwrap();
    let resumed = studio.transcript(&child).unwrap();
    assert_eq!(resumed.len(), 2);
    assert_eq!(resumed[0].text, "one");
    assert_eq!(resumed[1].text, "one");
}

#[tokio::test]
async fn fork_at_user_node_stops_before_the_original_assistant_reply() {
    let dir = tmpdir("user");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .create_agent(
            Some("source".into()),
            "mock".into(),
            "mock-1".into(),
            Some("/tmp".into()),
        )
        .unwrap();
    studio
        .send_message("source", "one".into(), "u1".into())
        .await
        .unwrap();

    let result = studio
        .fork_session(
            "source",
            json!({ "role": "user", "entryId": "studio-entry:0:user" }),
        )
        .await
        .unwrap();
    let child = result["sessionId"].as_str().unwrap().to_string();

    let transcript = studio.transcript(&child).unwrap();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].role, "user");
    assert_eq!(transcript[0].text, "one");

    studio.soft_unbind_agent(&child).unwrap();
    studio.resume_session(&child).unwrap();
    let resumed = studio.transcript(&child).unwrap();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].role, "user");
    assert_eq!(resumed[0].text, "one");
}

#[tokio::test]
async fn retry_rewinds_same_session_and_replaces_the_selected_turn_durably() {
    let dir = tmpdir("retry");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .create_agent(
            Some("source".into()),
            "mock".into(),
            "mock-1".into(),
            Some("/tmp".into()),
        )
        .unwrap();
    studio
        .send_message("source", "one".into(), "u1".into())
        .await
        .unwrap();
    studio
        .send_message("source", "two".into(), "u2".into())
        .await
        .unwrap();

    let result = studio
        .retry_session_turn(
            "source",
            json!({ "role": "user", "entryId": "studio-entry:2:user" }),
            Some("two edited".into()),
            Some("u2-edited".into()),
        )
        .await
        .unwrap();
    assert_eq!(result["sessionId"], "source");
    assert_eq!(result["retried"], true);

    let transcript = studio.transcript("source").unwrap();
    let visible: Vec<_> = transcript
        .iter()
        .map(|message| (message.role.as_str(), message.text.as_str()))
        .collect();
    assert_eq!(
        visible,
        vec![
            ("user", "one"),
            ("assistant", "one"),
            ("user", "two edited"),
            ("assistant", "two edited"),
        ],
    );
    assert!(!transcript.iter().any(|message| message.text == "two"));

    studio.soft_unbind_agent("source").unwrap();
    studio.resume_session("source").unwrap();
    let resumed = studio.transcript("source").unwrap();
    let resumed_visible: Vec<_> = resumed
        .iter()
        .map(|message| (message.role.as_str(), message.text.as_str()))
        .collect();
    assert_eq!(resumed_visible, visible);
}
