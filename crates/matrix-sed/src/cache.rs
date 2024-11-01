use std::future::Future;

use matrix_sdk::{
    deserialized_responses::SyncTimelineEvent, room::edit::EditError, ruma::EventId, Room,
};
use tracing::{debug, trace};

pub trait EventSource {
    fn get_event(
        &self,
        event_id: &EventId,
    ) -> impl Future<Output = Result<SyncTimelineEvent, EditError>>;
}

impl<'a> EventSource for &'a Room {
    async fn get_event(&self, event_id: &EventId) -> Result<SyncTimelineEvent, EditError> {
        match self.event_cache().await {
            Ok((event_cache, _drop_handles)) => {
                if let Some(event) = event_cache.event(event_id).await {
                    return Ok(event);
                }
                // Fallthrough: try with /event.
            }

            Err(err) => {
                debug!("error when getting the event cache: {err}");
            }
        }

        trace!("trying with /event now");
        self.event(event_id, None)
            .await
            .map(Into::into)
            .map_err(|err| EditError::Fetch(Box::new(err)))
    }
}
