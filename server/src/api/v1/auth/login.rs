use argon2::{Argon2, PasswordHash};
use http::StatusCode;
use poem::{
    handler,
    web::{
        cookie::{Cookie, CookieJar, SameSite},
        Data, Form,
    },
    IntoResponse, Response,
};
use rand::{
    distributions::{Alphanumeric, DistString},
    rngs::OsRng,
};
use serde::Deserialize;
use sqlx::{query, Postgres};
use tracing::instrument;

use crate::api::{
    sql_tx::Tx,
    v1::auth::{AuthServiceData, Claims, SESSION_COOKIE_NAME},
    ApiError,
};

#[derive(Deserialize)]
pub struct LoginForm {
    name: String,
    password: String,
}

#[handler]
#[instrument(skip(tx, form, cookies, data))]
pub async fn login(
    mut tx: Tx<Postgres>,
    Form(form): Form<LoginForm>,
    cookies: &CookieJar,
    Data(data): Data<&AuthServiceData>,
) -> Result<Response, ApiError> {
    let rec = query!("select uid,password from users where name = $1", form.name)
        .fetch_optional(&mut tx)
        .await?;
    Ok(match rec {
        Some(rec) => {
            let hash =
                PasswordHash::parse(&rec.password, argon2::password_hash::Encoding::default())?;
            hash.verify_password(&[&Argon2::default()], form.password.as_bytes())?;
            let session_key = Alphanumeric.sample_string(&mut OsRng, 96);
            query!(
                "insert into sessions(uid, user_id) values ($1, $2)",
                session_key,
                rec.uid,
            )
            .execute(&mut tx)
            .await?;
            let claims = Claims {
                sid: session_key,
                iss: "authust".to_owned(),
                sub: Some(rec.uid),
                authenticated: true,
            };
            let token = jsonwebtoken::encode(
                &jsonwebtoken::Header::default(),
                &claims,
                &data.encoding_key,
            )
            .expect("JWT encoding failed");
            let mut cookie = Cookie::new_with_str(SESSION_COOKIE_NAME, token);
            cookie.set_path("/");
            cookie.set_same_site(Some(SameSite::Strict));
            cookie.set_http_only(true);
            cookies.add(cookie);
            StatusCode::OK.into_response()
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    })
}