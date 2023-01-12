use std::fmt;

/// Options for pagination.
pub struct PaginationOptions<'a> {
    inner: PaginationOptionsInner<'a>,
}

impl<'a> PaginationOptions<'a> {
    /// Do a single pagination request, asking the server for the given
    /// maximum number of events.
    ///
    /// The server may choose to return fewer events, even if the start or end
    /// of the visible timeline is not yet reached.
    pub fn single_request(event_limit: u16) -> Self {
        Self::new(PaginationOptionsInner::SingleRequest { event_limit_if_first: Some(event_limit) })
    }

    /// Continually paginate with a fixed `limit` until at least the given
    /// amount of timeline items have been added (unless the start or end of the
    /// visible timeline is reached).
    ///
    /// The `event_limit` represents the maximum number of events the server
    /// should return in one batch. It may choose to return fewer events per
    /// response.
    pub fn until_num_items(event_limit: u16, items: u16) -> Self {
        Self::new(PaginationOptionsInner::UntilNumItems { event_limit, items })
    }

    /// Paginate once with the given initial maximum number of events, then
    /// do more requests based on the user-provided strategy
    /// callback.
    ///
    /// The callback is given numbers on the events and resulting timeline
    /// items for the last request as well as summed over all
    /// requests in a `paginate_backwards` call, and can decide
    /// whether to do another request (by returning
    /// `Some(next_event_limit)`) or not (by returning `None`).
    pub fn custom(
        initial_event_limit: u16,
        pagination_strategy: impl FnMut(PaginationOutcome) -> Option<u16> + Send + 'a,
    ) -> Self {
        Self::new(PaginationOptionsInner::Custom {
            event_limit_if_first: Some(initial_event_limit),
            strategy: Box::new(pagination_strategy),
        })
    }

    pub(super) fn next_event_limit(
        &mut self,
        pagination_outcome: PaginationOutcome,
    ) -> Option<u16> {
        match &mut self.inner {
            PaginationOptionsInner::SingleRequest { event_limit_if_first } => {
                event_limit_if_first.take()
            }
            PaginationOptionsInner::UntilNumItems { items, event_limit } => {
                (pagination_outcome.total_items_added < *items).then_some(*event_limit)
            }
            PaginationOptionsInner::Custom { event_limit_if_first, strategy } => {
                event_limit_if_first.take().or_else(|| strategy(pagination_outcome))
            }
        }
    }

    fn new(inner: PaginationOptionsInner<'a>) -> Self {
        Self { inner }
    }
}

pub enum PaginationOptionsInner<'a> {
    SingleRequest {
        event_limit_if_first: Option<u16>,
    },
    UntilNumItems {
        event_limit: u16,
        items: u16,
    },
    Custom {
        event_limit_if_first: Option<u16>,
        strategy: Box<dyn FnMut(PaginationOutcome) -> Option<u16> + Send + 'a>,
    },
}

impl<'a> fmt::Debug for PaginationOptions<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            PaginationOptionsInner::SingleRequest { event_limit_if_first } => f
                .debug_struct("SingleRequest")
                .field("event_limit_if_first", event_limit_if_first)
                .finish(),
            PaginationOptionsInner::UntilNumItems { items, event_limit } => f
                .debug_struct("UntilNumItems")
                .field("items", items)
                .field("event_limit", event_limit)
                .finish(),
            PaginationOptionsInner::Custom { event_limit_if_first, .. } => f
                .debug_struct("Custom")
                .field("event_limit_if_first", event_limit_if_first)
                .finish(),
        }
    }
}

/// The result of a successful pagination request.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct PaginationOutcome {
    /// The number of events received in last pagination response.
    pub events_received: u16,

    /// The number of timeline items added by the last pagination response.
    pub items_added: u16,

    /// The number of timeline items updated by the last pagination
    /// response.
    pub items_updated: u16,

    /// The number of events received by a `paginate_backwards` call so far.
    pub total_events_received: u16,

    /// The total number of items added by a `paginate_backwards` call so
    /// far.
    pub total_items_added: u16,

    /// The total number of items updated by a `paginate_backwards` call so
    /// far.
    pub total_items_updated: u16,
}

impl PaginationOutcome {
    pub(super) fn new() -> Self {
        Self {
            events_received: 0,
            items_added: 0,
            items_updated: 0,
            total_events_received: 0,
            total_items_added: 0,
            total_items_updated: 0,
        }
    }
}
