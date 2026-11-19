//! Grounding: send a `Screenshot` to a vision model and read back the pixel
//! coordinates of the UI element the agent named.
//!
//! The model answers in **image pixels** (the downscaled image it was shown),
//! which `Screenshot::map_point` turns into logical screen points.

use anyhow::{anyhow, Result};

use crate::provider::{ChatMessage, ChatRequest, Provider};
use crate::screenshot::Screenshot;

/// A grounded point in image-pixel space, plus the element it resolved to.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundingPoint {
    pub target: String,
    pub x: f64,
    pub y: f64,
}

/// Ask *provider* (using *model*) to locate *target* in *shot*.
pub async fn ground(
    provider: &dyn Provider,
    model: &str,
    shot: &Screenshot,
    target: &str,
) -> Result<GroundingPoint> {
    let prompt = format!(
        "You are a UI grounding assistant. Look at this screenshot of a computer \
         screen and locate the UI element described below. The image is {w}x{h} \
         pixels. Return ONLY a JSON object with exactly this shape: \
         {{\"target\": \"<element name>\", \"x\": <number>, \"y\": <number>}} where x \
         and y are pixel coordinates within this {w}x{h} image. No prose, no code \
         fences.\n\nTarget: {target}",
        w = shot.image_width(),
        h = shot.image_height(),
        target = target,
    );

    let req = ChatRequest {
        model: model.to_string(),
        messages: vec![ChatMessage::user_with_image(prompt, shot.data_uri())],
        tools: None,
        stream: false,
        temperature: Some(0.0),
        max_tokens: Some(200),
    };

    let response = provider.chat(req).await?;
    parse_grounding_response(&response)
}

/// Parse a grounding reply. Tolerates ```json fences and surrounding prose by
/// narrowing to the outermost JSON object, then parses exactly once.
pub fn parse_grounding_response(response: &str) -> Result<GroundingPoint> {
    let trimmed = response.trim();

    // Drop a markdown fence if present.
    let body = if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let end = after.find("```").unwrap_or(after.len());
        after[..end].trim()
    } else {
        trimmed
    };

    // Narrow to the outermost JSON object if the model added prose around it.
    let json_str = match (body.find('{'), body.rfind('}')) {
        (Some(a), Some(b)) if b > a => &body[a..=b],
        _ => body,
    };

    let value: serde_json::Value = serde_json::from_str(json_str)?;
    let target = value["target"]
        .as_str()
        .ok_or_else(|| anyhow!("missing 'target' field"))?
        .to_string();
    let x = value["x"]
        .as_f64()
        .ok_or_else(|| anyhow!("missing 'x' field"))?;
    let y = value["y"]
        .as_f64()
        .ok_or_else(|| anyhow!("missing 'y' field"))?;

    Ok(GroundingPoint { target, x, y })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatRequest, ChatStream};
    use async_trait::async_trait;

    struct MockProvider {
        response: String,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            unimplemented!()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    fn shot() -> Screenshot {
        Screenshot::test_screenshot(512, 320, 1440.0, 900.0)
    }

    #[test]
    fn test_parse_plain_json() {
        let p = parse_grounding_response(r#"{"target":"Icon","x":50,"y":75}"#).unwrap();
        assert_eq!(p, GroundingPoint { target: "Icon".into(), x: 50.0, y: 75.0 });
    }

    #[test]
    fn test_parse_fenced_json() {
        let p = parse_grounding_response("```json\n{\"target\":\"Icon\",\"x\":50,\"y\":75}\n```")
            .unwrap();
        assert_eq!(p.target, "Icon");
        assert_eq!(p.x, 50.0);
        assert_eq!(p.y, 75.0);
    }

    #[test]
    fn test_parse_with_surrounding_prose() {
        let p = parse_grounding_response("Here you go: {\"target\":\"OK\",\"x\":1.5,\"y\":2.5} done")
            .unwrap();
        assert_eq!(p.x, 1.5);
        assert_eq!(p.y, 2.5);
    }

    #[test]
    fn test_parse_float_coords() {
        let p = parse_grounding_response(r#"{"target":"t","x":12.25,"y":33.75}"#).unwrap();
        assert_eq!(p.x, 12.25);
        assert_eq!(p.y, 33.75);
    }

    #[test]
    fn test_parse_invalid_json_is_error() {
        assert!(parse_grounding_response("not valid json").is_err());
    }

    #[test]
    fn test_parse_missing_fields_is_error() {
        assert!(parse_grounding_response(r#"{"target":"Button"}"#).is_err());
        assert!(parse_grounding_response(r#"{"x":1,"y":2}"#).is_err());
    }

    #[tokio::test]
    async fn test_ground_returns_point_in_image_space() {
        let provider = MockProvider {
            response: r#"{"target":"Button","x":100,"y":200}"#.to_string(),
        };
        let p = ground(&provider, "vl-model", &shot(), "the blue button").await.unwrap();
        assert_eq!(p.target, "Button");
        assert_eq!(p.x, 100.0);
        assert_eq!(p.y, 200.0);
    }

    #[tokio::test]
    async fn test_ground_maps_to_screen_coords() {
        let provider = MockProvider {
            response: r#"{"target":"Button","x":256,"y":160}"#.to_string(),
        };
        let s = shot();
        let p = ground(&provider, "vl-model", &s, "button").await.unwrap();
        // Image center -> logical center (720, 450).
        assert_eq!(s.map_point(p.x, p.y), (720, 450));
    }
}