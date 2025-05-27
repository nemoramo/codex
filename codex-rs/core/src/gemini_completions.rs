use std::time::Duration;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt, TryStreamExt};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::client_common::{Prompt, ResponseEvent, ResponseStream};
use crate::error::{CodexErr, Result};
use crate::flags::{OPENAI_REQUEST_MAX_RETRIES, OPENAI_STREAM_IDLE_TIMEOUT_MS}; // Re-evaluate if these flags are appropriate for Gemini
use crate::model_provider_info::ModelProviderInfo;
use crate::models::{ContentItem, ResponseItem}; // Ensure these can be adapted or new structs are created if needed
use crate::util::backoff;

// Define structs for Gemini API request and response
// Based on https://ai.google.dev/gemini-api/docs/reference/rest/v1beta/models/generateContent
#[derive(serde::Serialize, Debug)]
struct GeminiRequestContents {
    parts: Vec<GeminiRequestPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>, // Optional: "user" or "model"
}

#[derive(serde::Serialize, Debug)]struct GeminiRequestPart {
    text: String,
}

#[derive(serde::Serialize, Debug)]
struct GeminiGenerationConfig {
    // Add fields if needed, e.g., temperature, maxOutputTokens
    // For now, keeping it simple
}

#[derive(serde::Serialize, Debug)]
struct GeminiSafetySetting {
    category: String,
    threshold: String,
}

#[derive(serde::Serialize, Debug)]
struct GeminiApiRequest {
    contents: Vec<GeminiRequestContents>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    safety_settings: Option<Vec<GeminiSafetySetting>>,
    // tools: Vec<Tool> - Not implementing tools for now
}

// Response parsing structs
#[derive(Deserialize, Debug)]
struct GeminiApiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "promptFeedback")]
    prompt_feedback: Option<GeminiPromptFeedback>,
}

#[derive(Deserialize, Debug)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    // other fields like finishReason, safetyRatings, citationMetadata
}

#[derive(Deserialize, Debug)]
struct GeminiContent {
    parts: Option<Vec<GeminiPart>>,
    role: Option<String>, // "model"
}

#[derive(Deserialize, Debug)]
struct GeminiPart {
    text: Option<String>,
    // other fields like inlineData for images
}

#[derive(Deserialize, Debug)]
struct GeminiPromptFeedback {
    // safetyRatings, etc.
}


pub(crate) async fn stream_gemini_content(
    prompt: &Prompt,
    model: &str, // This will be like "gemini-1.5-pro-latest"
    client: &reqwest::Client,
    provider: &ModelProviderInfo,
) -> Result<ResponseStream> {
    // 1. Construct messages/contents for Gemini
    // Gemini expects a specific "contents" structure.
    // System prompts might need to be prepended to the first user message,
    // or handled according to Gemini's best practices if it supports a direct system role equivalent.
    // For now, we'll concatenate system prompt and first user message.
    let mut gemini_contents: Vec<GeminiRequestContents> = Vec::new();
    let mut current_parts: Vec<GeminiRequestPart> = Vec::new();
    let mut system_prompt_text = String::new();

    if !prompt.get_full_instructions().is_empty() {
        system_prompt_text.push_str(&prompt.get_full_instructions());
        system_prompt_text.push('
'); // Separator
    }

    // Simplified prompt conversion:
    // Iterate through prompt.input, which are ResponseItems.
    // Concatenate text from user messages. Model messages in prompt.input are history.
    // This part needs careful adaptation from OpenAI's message format to Gemini's "contents" format.
    // Gemini's format is more like a sequence of user/model turns.
    // Example: contents: [ {role: "user", parts: [{text: "..."}]}, {role: "model", parts: [{text: "..."}]} ]

    let mut is_first_user_message = true;
    for item in &prompt.input {
        if let ResponseItem::Message { role, content } = item {
            let mut message_text = String::new();
            for c_item in content {
                if let ContentItem::InputText { text } | ContentItem::OutputText { text } = c_item {
                    message_text.push_str(text);
                }
                // TODO: Handle images (ContentItem::InputImage) if Gemini API supports them in this way
            }

            if !message_text.is_empty() {
                if role == "user" {
                    if is_first_user_message {
                        current_parts.push(GeminiRequestPart { text: format!("{}{}", system_prompt_text, message_text) });
                        is_first_user_message = false;
                    } else {
                        current_parts.push(GeminiRequestPart { text: message_text });
                    }
                    gemini_contents.push(GeminiRequestContents { parts: std::mem::take(&mut current_parts), role: Some("user".to_string()) });
                } else if role == "assistant" || role == "model" { // OpenAI uses "assistant", Gemini uses "model" for history
                     current_parts.push(GeminiRequestPart { text: message_text });
                     gemini_contents.push(GeminiRequestContents { parts: std::mem::take(&mut current_parts), role: Some("model".to_string()) });
                }
            }
        }
    }
     // If there's any remaining system prompt text and no user messages, add it as a user message.
    if is_first_user_message && !system_prompt_text.is_empty() {
        current_parts.push(GeminiRequestPart { text: system_prompt_text });
        gemini_contents.push(GeminiRequestContents { parts: std::mem::take(&mut current_parts), role: Some("user".to_string()) });
    }


    let api_request = GeminiApiRequest {
        contents: gemini_contents,
        generation_config: None, // Add config if needed
        safety_settings: None,   // Add settings if needed
    };

    let api_key = provider.api_key()?.ok_or_else(|| {
        CodexErr::EnvVar(crate::error::EnvVarError {
            var: provider.env_key.clone().unwrap_or_default(),
            instructions: provider.env_key_instructions.clone(),
        })
    })?;

    // Note: provider.base_url is "https://generativelanguage.googleapis.com/"
    // The model string (e.g., "gemini-1.5-pro-latest") is part of the path for Gemini.
    let url = format!(
        "{}v1beta/models/{}:streamGenerateContent?key={}",
        provider.base_url, model, api_key
    );

    debug!(url, "POST (Gemini)");
    trace!("Gemini request payload: {:?}", api_request);

    let mut attempt = 0;
    loop {
        attempt += 1;

        let res = client
            .post(&url)
            .json(&api_request) // Send struct that serializes to the correct JSON
            .send()
            .await;

        match res {
            Ok(resp) if resp.status().is_success() => {
                let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(16);
                let stream = resp.bytes_stream().map_err(CodexErr::Reqwest);
                tokio::spawn(process_gemini_sse(stream, tx_event));
                return Ok(ResponseStream { rx_event });
            }
            Ok(res) => {
                let status = res.status();
                let body = res.text().await.unwrap_or_default();
                warn!(%status, %body, "Error from Gemini API");
                if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) {
                    return Err(CodexErr::UnexpectedStatus(status, body));
                }
                if attempt > *OPENAI_REQUEST_MAX_RETRIES {
                    return Err(CodexErr::RetryLimit(status));
                }
                let delay = backoff(attempt); // Consider if backoff strategy needs adjustment
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                if attempt > *OPENAI_REQUEST_MAX_RETRIES {
                    return Err(e.into());
                }
                let delay = backoff(attempt);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn process_gemini_sse<S>(stream: S, tx_event: mpsc::Sender<Result<ResponseEvent>>)
where
    S: Stream<Item = Result<Bytes>> + Unpin,
{
    let mut stream = stream.eventsource();
    let idle_timeout = *OPENAI_STREAM_IDLE_TIMEOUT_MS; // Re-evaluate timeout for Gemini

    loop {
        let sse_event_result = timeout(idle_timeout, stream.next()).await;

        match sse_event_result {
            Ok(Some(Ok(sse))) => {
                if sse.event == "message" { // Gemini SSE might not use named events like OpenAI, often it's just data
                    // Or it might be just sse.data without checking sse.event if not applicable
                }

                if sse.data.is_empty() { // Sometimes empty messages are sent as keep-alives
                    continue;
                }
                
                // Gemini streams an array of chunks, often the actual data is inside this array.
                // The stream is a series of JSON objects. Each JSON object is a GeminiApiResponse.
                // Unlike OpenAI, Gemini doesn't usually stream word by word but larger chunks.
                match serde_json::from_str::<GeminiApiResponse>(&sse.data) {
                    Ok(gemini_response) => {
                        if let Some(candidates) = gemini_response.candidates {
                            for candidate in candidates {
                                if let Some(content) = candidate.content {
                                    if let Some(parts) = content.parts {
                                        for part in parts {
                                            if let Some(text) = part.text {
                                                let item = ResponseItem::Message {
                                                    role: "assistant".to_string(), // Or content.role if available and matches "model"
                                                    content: vec![ContentItem::OutputText { text }],
                                                };
                                                if tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await.is_err() {
                                                    return; // Receiver dropped
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to parse Gemini SSE data: {:?}, data: {}", e, sse.data);
                        // Decide if to send error or continue
                    }
                }
            }
            Ok(Some(Err(e))) => { // Error from the eventsource stream itself
                let _ = tx_event.send(Err(CodexErr::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => { // Stream ended from the server
                let _ = tx_event.send(Ok(ResponseEvent::Completed { response_id: String::new() /* Gemini doesn't have a response_id like OpenAI's /responses API */ })).await;
                return;
            }
            Err(_) => { // Timeout
                let _ = tx_event.send(Err(CodexErr::Stream("idle timeout waiting for Gemini SSE".into()))).await;
                return;
            }
        }
    }
}
