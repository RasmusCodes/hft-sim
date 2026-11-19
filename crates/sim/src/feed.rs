//! Market-data feeds: private to one subscriber, or shared by a group.
//!
//! A feed carries one instrument's public events over one link and keeps one book view built
//! from them.
//!
//! * A **private** feed (`AgentConfig::subscribe`) has one member and uses that member's own
//!   `feed` latency. Every subscriber pays for its own deliveries and its own book copy.
//! * A **shared feed group** (`SimBuilder::add_feed_group`) stands for subscribers at one location
//!   receiving the same multicast packets. Every member gets each packet at the same moment over
//!   the group's link and reads the same book view, so per-packet work is paid once per group, not
//!   once per member. Jitter is common to the group, as it is for co-located receivers of one
//!   multicast stream.
//!
//! Each member chooses a [`FeedMode`]:
//! * `Callback`: `on_market_data` for every packet (event-driven strategies);
//! * `Snapshot`: no per-packet callbacks; the member reads its view when it acts
//!   (timer-driven strategies). The view is the same one `Callback` members see, so a
//!   snapshot member pays nothing per packet.

use lob::MboBook;

use crate::latency::Link;
use crate::messages::{AgentId, InstrumentId};

/// Index of a shared feed group.
pub type FeedGroupId = u32;

/// Index of a feed inside the kernel.
pub(crate) type FeedId = u32;

/// How a subscriber consumes market data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FeedMode {
    /// `on_market_data` is called for every packet.
    Callback,
    /// No per-packet callbacks; read `ctx.book(..)` when acting.
    Snapshot,
}

/// One market-data subscription of an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Subscription {
    /// Instrument.
    pub instrument: InstrumentId,
    /// Callback or snapshot.
    pub mode: FeedMode,
    /// Shared feed group, or `None` for a private feed over the agent's own feed link.
    pub group: Option<FeedGroupId>,
}

/// One feed: one instrument, one link, one book view, its members.
pub(crate) struct Feed {
    pub instrument: InstrumentId,
    pub link: Link,
    pub book: MboBook,
    /// Members in subscription order.
    pub members: Vec<(AgentId, FeedMode)>,
}
