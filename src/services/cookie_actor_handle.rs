use ractor::Actor;
use snafu::{GenerateImplicitData, Location};

use crate::{
    config::{CookieStatus, Reason},
    error::ClewdrError,
};

use super::cookie_actor::{
    CookieActor, CookieActorHandle, CookieActorMessage, CookieRequestScope, CookieStatusInfo,
    INTERVAL,
};

impl CookieActorHandle {
    pub async fn start() -> Result<Self, ractor::SpawnErr> {
        let (actor_ref, _) = Actor::spawn(None, CookieActor, ()).await?;
        let handle = Self {
            actor_ref: actor_ref.clone(),
        };
        handle.spawn_timeout_checker();
        Ok(handle)
    }

    fn spawn_timeout_checker(&self) {
        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(INTERVAL));
            loop {
                interval.tick().await;
                if ractor::cast!(actor_ref, CookieActorMessage::CheckReset).is_err() {
                    break;
                }
            }
        });
    }

    pub async fn request(&self, cache_hash: Option<u64>) -> Result<CookieStatus, ClewdrError> {
        self.request_with_scope(CookieRequestScope::Any, cache_hash)
            .await
    }

    pub async fn request_fable(
        &self,
        cache_hash: Option<u64>,
    ) -> Result<CookieStatus, ClewdrError> {
        self.request_with_scope(CookieRequestScope::Fable, cache_hash)
            .await
    }

    async fn request_with_scope(
        &self,
        scope: CookieRequestScope,
        cache_hash: Option<u64>,
    ) -> Result<CookieStatus, ClewdrError> {
        ractor::call!(
            self.actor_ref,
            CookieActorMessage::Request,
            scope,
            cache_hash
        )
        .map_err(|error| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!("Failed to request a cookie from CookieActor: {error}"),
        })?
    }

    pub async fn return_cookie(
        &self,
        cookie: CookieStatus,
        reason: Option<Reason>,
    ) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, CookieActorMessage::Return(cookie, reason)).map_err(|error| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to return a cookie to CookieActor: {error}"),
            }
        })
    }

    pub async fn submit(&self, cookie: CookieStatus) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, CookieActorMessage::Submit(cookie)).map_err(|error| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to submit a cookie to CookieActor: {error}"),
            }
        })
    }

    pub async fn get_status(&self) -> Result<CookieStatusInfo, ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::GetStatus).map_err(|error| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to get CookieActor status: {error}"),
            }
        })
    }

    pub async fn delete_cookie(&self, cookie: CookieStatus) -> Result<(), ClewdrError> {
        ractor::call!(self.actor_ref, CookieActorMessage::Delete, cookie).map_err(|error| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!("Failed to delete a cookie from CookieActor: {error}"),
            }
        })?
    }
}
