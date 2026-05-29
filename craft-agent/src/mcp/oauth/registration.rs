use reqwest::Client;
use serde_json::json;

use super::OAuthError;

pub struct ClientRegistration {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub client_secret_expires_at: Option<u64>,
}

pub async fn register_client(
    client: &Client,
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<ClientRegistration, OAuthError> {
    let body = json!({
        "client_name": "Craft",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });

    let response = client
        .post(registration_endpoint)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).map_err(|e| OAuthError::Other(e.to_string()))?)
        .send()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let body_str = response.text().await.unwrap_or_default();
        return Err(OAuthError::ServerRejected {
            status: status.as_u16(),
            body: body_str,
        });
    }

    let body_str = response
        .text()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;

    let resp: serde_json::Value =
        serde_json::from_str(&body_str).map_err(|e| OAuthError::InvalidResponse(e.to_string()))?;

    let client_id = resp["client_id"]
        .as_str()
        .ok_or_else(|| {
            OAuthError::InvalidResponse("missing client_id in registration response".into())
        })?
        .to_string();
    let client_secret = resp["client_secret"].as_str().map(String::from);
    let client_secret_expires_at = resp["client_secret_expires_at"].as_u64();

    Ok(ClientRegistration {
        client_id,
        client_secret,
        client_secret_expires_at,
    })
}
