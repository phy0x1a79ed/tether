//! The relay host's side of issuing, for tests: what the relay host's issuer
//! does with the bearer it alone holds, against a relay the test started.
//!
//! Shared by the daemon's tests and the owner's reconnect tests, by path.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::json;
use tether_link::http::{self, HttpError};
use tether_link::resume::Restore;
use tether_link::{LinkError, Relay};
use tether_operator::issuer::{self, Issued, Issuer};
use tether_proto::invite::Slot;

pub struct RelayHost {
    pub relay: Relay,
    pub bearer: String,
}

impl RelayHost {
    pub fn issuer(relay: &Relay, bearer: &str) -> Arc<dyn Issuer> {
        Arc::new(Self {
            relay: relay.clone(),
            bearer: bearer.to_string(),
        })
    }
}

impl Issuer for RelayHost {
    fn issue(&self) -> BoxFuture<'static, Result<Issued, String>> {
        let (relay, bearer) = (self.relay.clone(), self.bearer.clone());
        Box::pin(async move {
            let answer = http::post_json(&relay, "/issue", Some(&bearer))
                .await
                .map_err(|e| e.to_string())?;
            issuer::issued(&answer)
        })
    }
}

impl Restore for RelayHost {
    fn restore(&self, slot: Slot, seat: &str, owner: &str) -> BoxFuture<'static, Result<(), LinkError>> {
        let (relay, bearer) = (self.relay.clone(), self.bearer.clone());
        let body = issuer::restore_request(slot, seat, owner).to_string();
        Box::pin(async move {
            let answer = match http::post_json_body(&relay, "/issue", Some(&bearer), &body).await {
                Ok(_) => json!({"restored": true}),
                Err(HttpError::Status(409)) => json!({"refused": "taken"}),
                Err(HttpError::Status(410)) => json!({"refused": "ended"}),
                Err(e) => return Err(e.into()),
            };
            issuer::restored(&answer)
        })
    }
}
