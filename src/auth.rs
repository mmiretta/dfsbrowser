//! Optional HTTP Basic authentication in front of every route.
//!
//! The S3 login inside the app only proves that the user holds credentials for
//! *some* bucket. Basic auth decides who may use this server at all (and
//! therefore who may make it open outbound S3 connections).

use crate::AppState;
use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{web, Error, HttpResponse};
use base64::Engine as _;
use sha2::{Digest, Sha256};

pub async fn basic_auth(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, Error> {
    let expected = req
        .app_data::<web::Data<AppState>>()
        .and_then(|s| s.settings.basic_auth.clone());

    let Some((user, pass)) = expected else {
        return Ok(next.call(req).await?.map_into_boxed_body());
    };

    // The liveness probe stays open so proxies and orchestrators can use it.
    if req.path() == "/healthz" || credentials_match(&req, &user, &pass) {
        return Ok(next.call(req).await?.map_into_boxed_body());
    }

    let resp = HttpResponse::Unauthorized()
        .insert_header((
            "WWW-Authenticate",
            r#"Basic realm="DFS Browser", charset="UTF-8""#,
        ))
        .body("authentication required");
    Ok(req.into_response(resp).map_into_boxed_body())
}

fn credentials_match(req: &ServiceRequest, user: &str, pass: &str) -> bool {
    let Some(header) = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(encoded) = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))
    else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((u, p)) = decoded.split_once(':') else {
        return false;
    };
    // Compare fixed-length digests so the comparison time does not depend on
    // how many leading bytes are correct. Evaluate both before combining.
    let user_ok = digest_eq(u, user);
    let pass_ok = digest_eq(p, pass);
    user_ok & pass_ok
}

fn digest_eq(a: &str, b: &str) -> bool {
    let da = Sha256::digest(a.as_bytes());
    let db = Sha256::digest(b.as_bytes());
    da.iter()
        .zip(db.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::digest_eq;

    #[test]
    fn digest_compare() {
        assert!(digest_eq("secret", "secret"));
        assert!(!digest_eq("secret", "secreT"));
        assert!(!digest_eq("", "x"));
    }
}
