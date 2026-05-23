//! Host identity values used by metadata and provider APIs.

use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::net::{Ipv4Addr, Ipv6Addr};

/// Host unique identifier. While this could be a trait, explicitly supported a fixed set makes it easier to store as a hash key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HostId {
    /// IPv4 address identity.
    IPv4(Ipv4Addr),
    /// IPv6 address identity.
    IPv6(Ipv6Addr),
    /// UUID v4 identity.
    UuidV4(uuid::Uuid),
    /// Numeric identity.
    U64(u64),
    /// Provider-defined string identity.
    String(String),
}

impl Display for HostId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            HostId::IPv4(addr) => write!(f, "{}", addr),
            HostId::IPv6(addr) => write!(f, "{}", addr),
            HostId::UuidV4(addr) => write!(f, "{}", addr),
            HostId::U64(addr) => write!(f, "{}", addr),
            HostId::String(addr) => write!(f, "{}", addr),
        }
    }
}
