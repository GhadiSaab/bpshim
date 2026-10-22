//! `ipn:<node>.<service>` endpoint identifiers (the only scheme in scope).

use std::fmt;

use bp7::eid::EndpointID;

use crate::error::{BpError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Eid {
    pub node: u32,
    pub service: u32,
}

impl Eid {
    pub fn new(node: u32, service: u32) -> Self {
        Eid { node, service }
    }

    /// Checks that the identifier can name an endpoint (node 0 is not a valid host).
    pub fn validate(&self) -> Result<()> {
        if self.node == 0 {
            return Err(BpError::invalid("node id must be greater than 0"));
        }
        Ok(())
    }

    pub(crate) fn to_bp7(self) -> Result<EndpointID> {
        EndpointID::with_ipn(self.node as u64, self.service as u64)
            .map_err(|e| BpError::invalid(format!("invalid ipn endpoint {}: {:?}", self, e)))
    }

    /// Maps a bundle's source onto an `Eid`; non-ipn sources become `ipn:0.0`.
    pub(crate) fn from_bp7(eid: &EndpointID) -> Eid {
        match eid {
            EndpointID::Ipn(_, addr) => Eid {
                node: addr.node_number() as u32,
                service: addr.service_number() as u32,
            },
            _ => Eid {
                node: 0,
                service: 0,
            },
        }
    }
}

impl fmt::Display for Eid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ipn:{}.{}", self.node, self.service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_as_ipn_uri() {
        assert_eq!(Eid::new(20, 1).to_string(), "ipn:20.1");
    }

    #[test]
    fn node_zero_is_rejected() {
        assert_eq!(Eid::new(0, 1).validate().unwrap_err().errno, libc::EINVAL);
    }

    #[test]
    fn bp7_round_trip() {
        let e = Eid::new(10, 2);
        assert_eq!(Eid::from_bp7(&e.to_bp7().unwrap()), e);
    }

    #[test]
    fn dtn_source_maps_to_zero() {
        let none = EndpointID::none();
        assert_eq!(Eid::from_bp7(&none), Eid::new(0, 0));
    }
}
