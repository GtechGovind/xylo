//! Tests for image generation: chat results and saved connections.

mod chat_results {
    use super::super::generated_tool_attachments;
    use uuid::Uuid;
    use xylo_core::{AttachmentSource, ImageAttachment, MAX_ATTACHMENTS_PER_TURN};
    fn attachment(session: Uuid) -> ImageAttachment {
        ImageAttachment {
            id: Uuid::new_v4(),
            session_id: session,
            name: "generated.png".into(),
            media_type: "image/png".into(),
            sha256: "a".repeat(64),
            bytes: 100,
            width: 16,
            height: 16,
            source: AttachmentSource::Generated,
            imported_at: 0,
        }
    }
    fn tool(images: Vec<ImageAttachment>) -> crate::state::ToolCard {
        let mut tool = crate::state::ToolCard::new(
            Uuid::new_v4(),
            "image_generate".into(),
            serde_json::json!({}),
            None,
        );
        tool.result = Some(
            serde_json::json!({"summary":{"images":images},"artifacts":["/untrusted/ignored.png"]})
                .to_string(),
        );
        tool.success = Some(true);
        tool.started_at = 0;
        tool.finished_at = Some(1);
        tool.refresh_presentation();
        tool
    }
    #[test]
    fn generated_chat_images_are_bounded_and_session_scoped() {
        let session = Uuid::new_v4();
        let images = (0..10).map(|_| attachment(session)).collect();
        assert_eq!(
            generated_tool_attachments(&tool(images), session).len(),
            MAX_ATTACHMENTS_PER_TURN
        );
        assert!(
            generated_tool_attachments(&tool(vec![attachment(Uuid::new_v4())]), session).is_empty()
        );
        let mut invalid = tool(vec![attachment(session)]);
        invalid.success = Some(false);
        assert!(generated_tool_attachments(&invalid, session).is_empty());
    }
    #[test]
    fn generated_chat_images_never_use_artifact_paths_as_previews() {
        let session = Uuid::new_v4();
        let mut invalid = tool(vec![attachment(session)]);
        invalid.result=Some(serde_json::json!({"summary":{"images":[{"path":"/untrusted/ignored.png"}]},"artifacts":["/untrusted/ignored.png"]}).to_string());
        invalid.refresh_presentation();
        assert!(generated_tool_attachments(&invalid, session).is_empty());
        invalid.name = "other_tool".into();
        invalid.result = tool(vec![attachment(session)]).result;
        invalid.refresh_presentation();
        assert!(generated_tool_attachments(&invalid, session).is_empty());
    }
}

mod saved_connections {
    use super::super::{
        ApiFamily, CHATGPT_REF, ProviderConfig, compatible_image_connection,
        detach_image_credential,
    };
    #[test]
    fn new_image_key_detaches_from_saved_chat_credentials_and_existing_turn_references() {
        let chat = ProviderConfig {
            endpoint: "https://api.example/v1".into(),
            credential_ref: Some("provider:shared-chat".into()),
            ..Default::default()
        };
        let mut image = compatible_image_connection(&chat).unwrap();
        let previous = image.credential_ref.clone();
        detach_image_credential(&mut image);
        assert_eq!(chat.credential_ref, previous);
        assert_ne!(image.credential_ref, previous);
        assert!(
            image
                .credential_ref
                .as_deref()
                .unwrap()
                .starts_with("image-provider:")
        );
        let admitted_turn_reference = image.credential_ref.clone();
        detach_image_credential(&mut image);
        assert_ne!(image.credential_ref, admitted_turn_reference);
    }
    #[test]
    fn saved_image_connections_reuse_only_compatible_api_credentials_and_never_a_chat_model() {
        for family in [
            ApiFamily::OpenAiChat,
            ApiFamily::OpenAiResponses,
            ApiFamily::Gemini,
        ] {
            let provider = ProviderConfig {
                name: "API fixture".into(),
                family,
                endpoint: "https://api.example/v1".into(),
                model: "a-chat-model".into(),
                credential_ref: Some("provider:fixture".into()),
                ..Default::default()
            };
            let image = compatible_image_connection(&provider).unwrap();
            assert!(image.model.is_empty());
            assert_eq!(image.credential_ref, provider.credential_ref);
            assert_eq!(image.endpoint, provider.endpoint);
            for account in [CHATGPT_REF, "account:another-plan"] {
                let mut account_provider = provider.clone();
                account_provider.credential_ref = Some(account.into());
                assert!(compatible_image_connection(&account_provider).is_none());
            }
        }
        for family in [
            ApiFamily::Anthropic,
            ApiFamily::Azure,
            ApiFamily::Bedrock,
            ApiFamily::Vertex,
        ] {
            let provider = ProviderConfig {
                family,
                endpoint: "https://api.example/v1".into(),
                ..Default::default()
            };
            assert!(compatible_image_connection(&provider).is_none());
        }
    }
}
