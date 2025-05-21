use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    extract::FromRequestParts,
    http::{Request, Response},
};
use futures::future::BoxFuture;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tower::{Layer, Service};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    aud: String,
    exp: i64,
    sub: String,
    email: String,
    role: String,
}

#[derive(Debug, Clone)]
pub struct AuthState {
    pub user_id: uuid::Uuid,
    #[allow(dead_code)]
    pub email: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

pub fn supabase_auth_layer<RB>(
    jwt_secret: String,
    supabase_url: String,
    supabase_anon_key: String,
) -> SupabaseAuthenticationLayer<RB>
where
    RB: From<String>,
{
    SupabaseAuthenticationLayer::<RB>::new(
        jwt_secret,
        format!("{}/auth/v1", supabase_url),
        supabase_anon_key,
        false,
    )
}

impl FromRequestParts<()> for AuthState {
    type Rejection = Response<String>;

    fn from_request_parts<'life0, 'life1, 'async_trait>(
        parts: &'life0 mut axum::http::request::Parts,
        _state: &'life1 (),
    ) -> Pin<Box<dyn Future<Output = Result<Self, Self::Rejection>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            parts.extensions.get().cloned().ok_or_else(|| {
                Response::builder()
                    .status(401)
                    .body("Unauthenticated".to_string())
                    .unwrap()
            })
        })
    }
}

pub struct SupabaseAuthenticationLayer<RB> {
    pub jwt_secret: DecodingKey,
    pub audience: String,
    pub supabase_anon_key: String,
    /// If true, the middleware will reject requests that do not have an authenticated user
    pub reject_no_auth: bool,
    _ty: PhantomData<fn() -> RB>,
}

impl<RB> SupabaseAuthenticationLayer<RB> {
    pub fn new(
        jwt_secret: String,
        supabase_url: String,
        supabase_anon_key: String,
        reject_no_auth: bool,
    ) -> Self {
        Self {
            jwt_secret: DecodingKey::from_secret(jwt_secret.as_bytes()),
            audience: supabase_url,
            supabase_anon_key,
            reject_no_auth,
            _ty: PhantomData,
        }
    }
}

impl<RB: From<String>> Clone for SupabaseAuthenticationLayer<RB> {
    fn clone(&self) -> Self {
        Self {
            jwt_secret: self.jwt_secret.clone(),
            audience: self.audience.clone(),
            supabase_anon_key: self.supabase_anon_key.clone(),
            reject_no_auth: self.reject_no_auth,
            _ty: PhantomData,
        }
    }
}

impl<S, RB> Layer<S> for SupabaseAuthenticationLayer<RB> {
    type Service = SupabaseAuthentication<S, RB>;

    fn layer(&self, inner: S) -> Self::Service {
        SupabaseAuthentication {
            inner,
            jwt_secret: self.jwt_secret.clone(),
            audience: self.audience.clone(),
            supabase_anon_key: self.supabase_anon_key.clone(),
            reject_no_auth: self.reject_no_auth,
            _ty: PhantomData,
        }
    }
}

pub struct SupabaseAuthentication<S, RB> {
    pub inner: S,
    pub jwt_secret: DecodingKey,
    pub audience: String,
    pub supabase_anon_key: String,
    pub reject_no_auth: bool,
    _ty: PhantomData<fn() -> RB>,
}

impl<S, RB> Clone for SupabaseAuthentication<S, RB>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            jwt_secret: self.jwt_secret.clone(),
            audience: self.audience.clone(),
            supabase_anon_key: self.supabase_anon_key.clone(),
            reject_no_auth: self.reject_no_auth,
            _ty: PhantomData,
        }
    }
}

impl<S, RB: From<String>> SupabaseAuthentication<S, RB> {}

impl<B, RB, S> Service<Request<B>> for SupabaseAuthentication<S, RB>
where
    B: Send + 'static,
    S: Service<Request<B>, Response = Response<RB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    RB: From<String> + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let audience = self.audience.clone();
        let supabase_anon_key = self.supabase_anon_key.clone();
        let jwt_secret = self.jwt_secret.clone();
        let reject_no_auth = self.reject_no_auth;
        let mut inner = self.inner.clone();

        Box::pin(async move {
            match validate(
                &audience,
                &supabase_anon_key,
                &jwt_secret,
                reject_no_auth,
                &mut req,
            )
            .await
            {
                Ok(cookies) => {
                    let mut response = inner.call(req).await?;
                    if let Some((access_token_cookie, refresh_token_cookie)) = cookies {
                        response
                            .headers_mut()
                            .append("Set-Cookie", access_token_cookie.try_into().unwrap());
                        response
                            .headers_mut()
                            .append("Set-Cookie", refresh_token_cookie.try_into().unwrap());
                    }
                    Ok(response)
                }
                Err(res) => Ok(res),
            }
        })
    }
}

async fn validate<B, RB: From<String>>(
    audience: &str,
    supabase_anon_key: &str,
    jwt_secret: &DecodingKey,
    reject_no_auth: bool,
    request: &mut Request<B>,
) -> Result<Option<(String, String)>, Response<RB>> {
    let mut access_token = None;
    let mut refresh_token = None;

    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_audience(&["authenticated"]);
    validation.set_issuer(&[audience]);

    // Extract tokens from Authorization header or cookies
    if let Some(authorization) = request.headers().get("Authorization") {
        let parts = authorization
            .to_str()
            .unwrap()
            .split("Bearer")
            .collect::<Vec<&str>>();
        if parts.len() >= 2 {
            access_token = Some(parts[1].trim().to_owned());
        }
    }

    // Check cookies if no Authorization header
    if access_token.is_none() {
        if let Some(cookie) = request.headers().get("Cookie") {
            let cookie_str = cookie.to_str().unwrap();
            let cookies: Vec<&str> = cookie_str.split(';').collect();

            for cookie in cookies {
                let cookie = cookie.trim();
                if let Some(token) = cookie.strip_prefix("apikey=") {
                    access_token = Some(token.to_string());
                } else if let Some(token) = cookie.strip_prefix("refresh_token=") {
                    refresh_token = Some(token.to_string());
                }
            }
        }
    }

    match access_token {
        Some(token) => {
            match decode::<Claims>(&token, jwt_secret, &validation) {
                Ok(token) => {
                    if token.claims.role == "authenticated" {
                        request.extensions_mut().insert(AuthState {
                            user_id: uuid::Uuid::parse_str(&token.claims.sub).unwrap(),
                            email: token.claims.email,
                        });

                        Ok(None)
                    } else {
                        tracing::trace!("Unauthorized");

                        Err(Response::builder()
                            .status(401)
                            .body(RB::from("Unauthorized".to_owned()))
                            .unwrap())
                    }
                }
                Err(e) => {
                    tracing::trace!("Invalid JWT token: {}", e);

                    // Try to refresh the token if we have a refresh token
                    if let Some(refresh_token) = refresh_token {
                        match refresh_access_token(audience, supabase_anon_key, &refresh_token)
                            .await
                        {
                            Ok(new_tokens) => {
                                // Set the new tokens in cookies
                                let access_token_cookie = format!(
                                    "apikey={}; Max-Age={}; Path=/",
                                    new_tokens.access_token, new_tokens.expires_in
                                );
                                let refresh_token_cookie = format!(
                                    "refresh_token={}; Max-Age={}; Path=/",
                                    new_tokens.refresh_token,
                                    7 * 24 * 60 * 60
                                );

                                // Validate the new access token
                                match decode::<Claims>(
                                    &new_tokens.access_token,
                                    jwt_secret,
                                    &validation,
                                ) {
                                    Ok(token) => {
                                        if token.claims.role == "authenticated" {
                                            request.extensions_mut().insert(AuthState {
                                                user_id: uuid::Uuid::parse_str(&token.claims.sub)
                                                    .unwrap(),
                                                email: token.claims.email,
                                            });

                                            return Ok(Some((
                                                access_token_cookie,
                                                refresh_token_cookie,
                                            )));
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to validate new access token: {}",
                                            e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to refresh token: {}", e);
                            }
                        }
                    }

                    Err(Response::builder()
                        .status(401)
                        .header("Set-Cookie", "apikey=; Max-Age=0; Path=/")
                        .header("Set-Cookie", "refresh_token=; Max-Age=0; Path=/")
                        .body(RB::from("Invalid JWT token".to_owned()))
                        .unwrap())
                }
            }
        }
        None => {
            tracing::trace!("No authorization token found");

            if let Some(refresh_token) = refresh_token {
                match refresh_access_token(audience, supabase_anon_key, &refresh_token).await {
                    Ok(new_tokens) => {
                        // Set the new tokens in cookies
                        let access_token_cookie = format!(
                            "apikey={}; Max-Age={}; Path=/",
                            new_tokens.access_token, new_tokens.expires_in
                        );
                        let refresh_token_cookie = format!(
                            "refresh_token={}; Max-Age={}; Path=/",
                            new_tokens.refresh_token,
                            7 * 24 * 60 * 60
                        );

                        // Validate the new access token
                        match decode::<Claims>(&new_tokens.access_token, jwt_secret, &validation) {
                            Ok(token) => {
                                if token.claims.role == "authenticated" {
                                    request.extensions_mut().insert(AuthState {
                                        user_id: uuid::Uuid::parse_str(&token.claims.sub).unwrap(),
                                        email: token.claims.email,
                                    });

                                    return Ok(Some((access_token_cookie, refresh_token_cookie)));
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to validate new access token: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to refresh token: {}", e);
                    }
                }
            }

            if reject_no_auth {
                Err(Response::builder()
                    .status(401)
                    .body(RB::from("No authorization token found".to_owned()))
                    .unwrap())
            } else {
                Ok(None)
            }
        }
    }
}

async fn refresh_access_token(
    audience: &str,
    supabase_anon_key: &str,
    refresh_token: &str,
) -> Result<RefreshResponse, reqwest::Error> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/token?grant_type=refresh_token", audience))
        .header("apikey", supabase_anon_key)
        .json(&serde_json::json!({
            "refresh_token": refresh_token,
        }))
        .send()
        .await?
        .json::<RefreshResponse>()
        .await?;

    Ok(response)
}
