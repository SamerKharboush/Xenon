use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{DynamicImage, GenericImageView, ImageFormat};
use std::io::Cursor;

use crate::events::PetState;
use crate::provider::Provider;

pub struct GroundingResult {
    pub success: bool,
    pub message: String,
    pub state: PetState,
}

pub async fn ground_and_click(
    provider: &dyn Provider,
    screenshot: &[u8],
) -> Result<GroundingResult> {
    let resized = resize_screenshot(screenshot, 512, 512)?;
    let base64_screenshot = BASE64.encode(&resized);

    let prompt = "You are a UI grounding assistant. Look at this screenshot and identify the UI element that matches the user's request. Return ONLY a JSON object with this exact format: {\"target\": \"element name\", \"x\": number, \"y\": number}. Do not include any other text.";

    let response = provider.chat_vision(&base64_screenshot, prompt).await?;

    let coords = parse_grounding_response(&response)?;

    let enigo = enigo::Enigo::new(&enigo::Settings::default())?;
    let mut enigo = enigo;

    enigo.move_mouse(coords.x, coords.y, enigo::Coordinate::Abs)?;
    enigo.button(enigo::Button::Left, enigo::ButtonFunction::Click)?;

    Ok(GroundingResult {
        success: true,
        message: format!(
            "Clicked on {} at ({}, {})",
            coords.target, coords.x, coords.y
        ),
        state: PetState::Idle,
    })
}

fn resize_screenshot(screenshot: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let img = image::load_from_memory(screenshot)?;
    let resized = img.resize_exact(width, height, image::imageops::FilterType::Lanczos3);

    let mut buf = Cursor::new(Vec::new());
    resized.write_to(&mut buf, ImageFormat::Png)?;
    Ok(buf.into_inner())
}

#[derive(Debug, Clone)]
struct GroundingCoords {
    target: String,
    x: i32,
    y: i32,
}

fn parse_grounding_response(response: &str) -> Result<GroundingCoords> {
    let trimmed = response.trim();

    if trimmed.starts_with("```json") {
        let start = trimmed.find('{').ok_or_else(|| anyhow!("No JSON found in response"))?;
        let end = trimmed.rfind('}').ok_or_else(|| anyhow!("No JSON end found"))?;
        let json_str = &trimmed[start..=end];

        let value: serde_json::Value = serde_json::from_str(json_str)?;

        let target = value["target"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'target' field"))?
            .to_string();
        let x = value["x"]
            .as_i64()
            .ok_or_else(|| anyhow!("Missing 'x' field"))? as i32;
        let y = value["y"]
            .as_i64()
            .ok_or_else(|| anyhow!("Missing 'y' field"))? as i32;

        Ok(GroundingCoords { target, x, y })
    } else {
        let value: serde_json::Value = serde_json::from_str(trimmed)?;

        let target = value["target"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'target' field"))?
            .to_string();
        let x = value["x"]
            .as_i64()
            .ok_or_else(|| anyhow!("Missing 'x' field"))? as i32;
        let y = value["y"]
            .as_i64()
            .ok_or_else(|| anyhow!("Missing 'y' field"))? as i32;

        Ok(GroundingCoords { target, x, y })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ChatRequest, ChatResponse, ChatRequestWithTools, ChatStream};
    use async_trait::async_trait;

    struct MockProvider {
        response: String,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            unimplemented!()
        }

        async fn chat_with_tools(
            &self,
            _req: ChatRequestWithTools,
        ) -> Result<ChatResponse> {
            unimplemented!()
        }

        async fn chat_vision(&self, _image: &str, _prompt: &str) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    fn create_test_png() -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(100, 100);
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn test_ground_and_click_success() {
        let provider = MockProvider {
            response: r#"{"target": "Button", "x": 100, "y": 200}"#.to_string(),
        };

        let screenshot = create_test_png();
        let result = ground_and_click(&provider, &screenshot).await;

        assert!(result.is_ok());
        let grounding_result = result.unwrap();
        assert!(grounding_result.success);
    }

    #[tokio::test]
    async fn test_parse_grounding_response_with_markdown() {
        let response = r#"```json
{"target": "Icon", "x": 50, "y": 75}
```"#;

        let coords = parse_grounding_response(response).unwrap();
        assert_eq!(coords.target, "Icon");
        assert_eq!(coords.x, 50);
        assert_eq!(coords.y, 75);
    }

    #[tokio::test]
    async fn test_parse_grounding_response_invalid_json() {
        let response = "not valid json";
        let result = parse_grounding_response(response);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_grounding_response_missing_fields() {
        let response = r#"{"target": "Button"}"#;
        let result = parse_grounding_response(response);
        assert!(result.is_err());
    }
}
