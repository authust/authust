use std::fmt::Debug;

use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts, State},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use deadpool_postgres::GenericClient;
use futures::future::BoxFuture;

use http::{request::Parts, Request};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation};
use model::user::PartialUser;
use once_cell::sync::Lazy;
use rand::{
    distributions::{Alphanumeric, DistString},
    rngs::OsRng,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use tower::{Layer, Service};
use tower_cookies::{cookie::SameSite, Cookie, Cookies};
use tracing::instrument;
use uuid::Uuid;

use crate::{
    api::{ApiError, ApiErrorKind, AuthServiceData},
    auth::Session,
    SharedState,
};

pub const SESSION_COOKIE_NAME: &str = "session";

pub(super) fn setup_auth_router() -> Router<SharedState> {
    Router::new().route("/", get(user_info))
}

#[derive(Serialize)]
pub struct SessionResponse {
    user: Option<PartialUser>,
}

#[axum::debug_handler]
pub async fn user_info(
    session: Session,
    State(state): State<SharedState>,
) -> Result<Json<SessionResponse>, ApiError> {
    let connection = state.defaults().connection().await?;
    let user = session.get_user(&connection, &state).await?;
    Ok(Json(SessionResponse { user }))
}

#[derive(Debug, Clone)]
pub struct AuthLayer {
    data: AuthServiceData,
}

impl AuthLayer {
    pub fn new(data: AuthServiceData) -> Self {
        Self { data }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            data: self.data.clone(),
            inner,
        }
    }
}

#[derive(Clone)]
pub struct AuthService<S> {
    data: AuthServiceData,
    inner: S,
}
impl<S, ReqBody: Send + 'static> Service<Request<ReqBody>> for AuthService<S>
where
    S: Service<Request<ReqBody>> + Send + Clone + 'static,
    S::Response: IntoResponse,
    S::Future: Send + 'static,
{
    type Response = Response;

    type Error = S::Error;

    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let mut inner = self.inner.clone();
        let data = self.data.clone();
        Box::pin(async move {
            if let Err(err) = handle_auth(&mut req, &data) {
                return Ok(err);
            }
            Ok(inner.call(req).await.map(IntoResponse::into_response)?)
        })
    }
}

pub enum AuthExtension {
    Valid(AuthExtensionData),
    MissingCookie,
}

#[derive(Debug, Clone)]
pub struct AuthExtensionData {
    pub header: jsonwebtoken::Header,
    pub claims: Claims,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Oauth2Claims {
    iss: String,
    sub: String,
    aud: String,
    exp: u64,
    iat: u64,
    auth_time: u64,
    acr: String,

    email: String,
    email_verified: bool,

    name: String,
    given_name: String,
    family_name: String,
    middle_name: String,
    nickname: String,
    preferred_username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sid: String,
    pub iss: String,
    pub sub: Option<Uuid>,
    pub authenticated: bool,
    pub is_admin: bool,
}

const VALIDATION: Lazy<Validation> = Lazy::new(|| {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_required_spec_claims(&["iss"]);
    validation.set_issuer(&["authust"]);
    validation
});

pub fn decode_token<T: DeserializeOwned>(
    key: &DecodingKey,
    token: &str,
) -> Result<TokenData<T>, ApiError> {
    Ok(jsonwebtoken::decode(token, key, &VALIDATION)?)
}

pub fn encode_token(key: &EncodingKey, claims: &Claims) -> Result<String, ApiError> {
    let header = Header::new(Algorithm::HS256);
    Ok(jsonwebtoken::encode(&header, claims, key)?)
}

pub fn set_session_cookie(
    key: &EncodingKey,
    cookies: &Cookies,
    claims: &Claims,
) -> Result<(), ApiError> {
    let token = encode_token(key, claims)?;
    let mut cookie = Cookie::new(SESSION_COOKIE_NAME, token);
    cookie.set_path("/");
    cookie.set_same_site(SameSite::Strict);
    cookie.set_http_only(true);
    cookies.add(cookie);
    Ok(())
}

fn handle_auth<B>(req: &mut Request<B>, data: &AuthServiceData) -> Result<(), Response> {
    let cookies: &Cookies = req.extensions().get().expect("Missing cookie layer");
    let Some(cookie) = cookies.get(SESSION_COOKIE_NAME) else {
        req.extensions_mut().insert(AuthExtension::MissingCookie);
        return Ok(()) };
    let value = cookie.value();
    let token = decode_token(&data.decoding_key, value).map_err(|err| err.into_response())?;
    req.extensions_mut()
        .insert(AuthExtension::Valid(AuthExtensionData {
            header: token.header,
            claims: token.claims,
        }));
    Ok(())
}

fn get_auth_extension_data(req: &mut Parts) -> Result<AuthExtensionData, ApiError> {
    let Some(extension): Option<&AuthExtension> = req.extensions.get() else {
            return Err(ApiErrorKind::MissingMiddleware("auth").into_api());
        };
    let data = match extension {
        AuthExtension::Valid(data) => data,
        AuthExtension::MissingCookie => {
            return Err(ApiErrorKind::SessionCookieMissing.into_api());
        }
    };
    Ok(data.to_owned())
}

async fn make_new_session<C: GenericClient>(
    client: &C,
    parts: &mut Parts,
    data: &AuthServiceData,
) -> Result<Session, ApiError> {
    let session_key = Alphanumeric.sample_string(&mut OsRng, 96);
    let statement = client
        .prepare_cached("insert into sessions(uid) values($1)")
        .await?;
    client.execute(&statement, &[&session_key]).await?;
    let claims = Claims {
        sid: session_key.clone(),
        iss: "authust".to_owned(),
        sub: None,
        authenticated: false,
        is_admin: false,
    };
    let cookies: &Cookies = parts.extensions.get().expect("Cookie layer is missing");
    set_session_cookie(&data.encoding_key, &cookies, &claims)?;
    Ok(Session {
        session_id: session_key,
        user_id: None,
        is_admin: false,
    })
}

#[async_trait]
impl FromRequestParts<SharedState> for Session {
    type Rejection = ApiError;

    #[instrument(skip(parts, state), name = "session")]
    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let connection = state.defaults().connection().await?;
        let data = match get_auth_extension_data(parts) {
            Ok(v) => Ok(v),
            Err(err) => match &err.kind {
                ApiErrorKind::SessionCookieMissing => {
                    return make_new_session(&connection, parts, state.auth_data()).await;
                }
                _ => Err(err),
            },
        }?;
        let claims = data.claims;
        let statement = connection
            .prepare_cached("select user_id from sessions where uid = $1")
            .await?;
        let res = connection
            .query_opt(&statement, &[&claims.sid])
            .await?
            .map(|v| v.get::<_, Option<Uuid>>(0));
        if let Some(user_id) = res {
            Ok(Session {
                session_id: claims.sid,
                user_id,
                is_admin: claims.is_admin,
            })
        } else {
            make_new_session(&connection, parts, state.auth_data()).await
        }
    }
}

#[repr(transparent)]
pub struct AdminSession(());

#[async_trait]
impl FromRequestParts<SharedState> for AdminSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state).await?;
        if session.user_id.is_some() && session.is_admin {
            Ok(AdminSession(()))
        } else {
            Err(ApiErrorKind::Forbidden.into_api())
        }
    }
}
