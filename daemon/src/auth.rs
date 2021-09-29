use hex::FromHexError;
use rocket::http::Status;
use rocket::outcome::{try_outcome, IntoOutcome};
use rocket::request::{FromRequest, Outcome};
use rocket::{Request, State};
use rocket_basicauth::BasicAuth;
use std::fmt;
use std::str::FromStr;

/// A request guard that can be included in handler definitions to enforce authentication.
pub struct Authenticated {}

pub const MAKER_USERNAME: &str = "maker";

#[derive(PartialEq)]
pub struct Password([u8; 32]);

impl From<[u8; 32]> for Password {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl FromStr for Password {
    type Err = FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(s, &mut bytes)?;

        Ok(Self(bytes))
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Authenticated {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let basic_auth = try_outcome!(req
            .guard::<BasicAuth>()
            .await
            .map_failure(|(status, error)| (status, {
                tracing::warn!("Bad basic auth header: {:?}", error);
            }))
            .forward_then(|()| Outcome::Failure((Status::Unauthorized, {
                tracing::warn!("No auth header");
            }))));
        let expected_password =
            try_outcome!(req
                .guard::<&'r State<Password>>()
                .await
                .map_failure(|(status, _)| (status, {
                    tracing::warn!("No password set in rocket state");
                })));

        if basic_auth.username != MAKER_USERNAME {
            tracing::warn!("Unknown user {}", basic_auth.username);

            return Outcome::Failure((Status::Unauthorized, ()));
        }

        let password = match basic_auth.password.parse::<Password>() {
            Ok(password) => password,
            Err(e) => {
                tracing::warn!("Failed to decode password: {}", e);

                return Outcome::Failure((Status::BadRequest, ()));
            }
        };

        if password != expected_password.inner() {
            tracing::warn!("Passwords don't match");

            return Outcome::Failure((Status::Unauthorized, ()));
        }

        Outcome::Success(Authenticated {})
    }
}
