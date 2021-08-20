use crate::{ServerRx, ServerTx};
use anyhow::{anyhow, Error, Result};
use linkerd_policy_controller_core::{
    ClientAuthentication, ClientAuthorization, IdentityMatch, InboundServer, IpNet, NetworkMatch,
    ProxyProtocol,
};
use linkerd_policy_controller_k8s_api as k8s;
use std::{collections::HashMap, hash::Hash};
use tokio::{sync::watch, time};

/// Indicates the default behavior to apply when no Server is found for a port.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum DefaultPolicy {
    Allow {
        /// Indicates that, by default, all traffic must be authenticated.
        authenticated_only: bool,

        /// Indicates that all traffic must, by default, be from an IP address within the cluster.
        cluster_only: bool,
    },

    /// Indicates that all traffic is denied unless explicitly permitted by an authorization policy.
    Deny,
}

/// Describes the default behavior for an individual pod to apply when no Server is found for a
/// port.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PodDefaults {
    default: DefaultPolicy,
    ports: HashMap<u16, PortDefaults>,
}

/// Describes the default behavior for a pod-port to apply when no Server is found for a port.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct PortDefaults {
    pub authenticated: bool,
    pub opaque: bool,
}

/// Holds the watches for all
#[derive(Debug)]
pub(crate) struct DefaultPolicyWatches {
    cluster_nets: Vec<IpNet>,
    detect_timeout: time::Duration,

    watches: HashMap<(DefaultPolicy, PortDefaults), (ServerTx, ServerRx)>,
}

// === impl PodDefaults ===

impl PodDefaults {
    pub fn new(
        default: DefaultPolicy,
        ports: impl IntoIterator<Item = (u16, PortDefaults)>,
    ) -> Self {
        Self {
            default,
            ports: ports.into_iter().collect(),
        }
    }
}

// === impl PortDefaults ===

impl From<DefaultPolicy> for PodDefaults {
    fn from(default: DefaultPolicy) -> Self {
        Self::new(default, None)
    }
}

// === impl DefaultPolicy ===

impl DefaultPolicy {
    pub const ANNOTATION: &'static str = "config.linkerd.io/default-inbound-policy";

    pub fn from_annotation(meta: &k8s::ObjectMeta) -> Result<Option<Self>> {
        if let Some(ann) = meta.annotations.as_ref() {
            if let Some(v) = ann.get(Self::ANNOTATION) {
                let mode = v.parse()?;
                return Ok(Some(mode));
            }
        }

        Ok(None)
    }
}

impl std::str::FromStr for DefaultPolicy {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "all-authenticated" => Ok(Self::Allow {
                authenticated_only: true,
                cluster_only: false,
            }),
            "all-unauthenticated" => Ok(Self::Allow {
                authenticated_only: false,
                cluster_only: false,
            }),
            "cluster-authenticated" => Ok(Self::Allow {
                authenticated_only: true,
                cluster_only: true,
            }),
            "cluster-unauthenticated" => Ok(Self::Allow {
                authenticated_only: false,
                cluster_only: true,
            }),
            "deny" => Ok(Self::Deny),
            s => Err(anyhow!("invalid mode: {:?}", s)),
        }
    }
}

impl std::fmt::Display for DefaultPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allow {
                authenticated_only: true,
                cluster_only: false,
            } => "all-authenticated".fmt(f),
            Self::Allow {
                authenticated_only: false,
                cluster_only: false,
            } => "all-unauthenticated".fmt(f),
            Self::Allow {
                authenticated_only: true,
                cluster_only: true,
            } => "cluster-authenticated".fmt(f),
            Self::Allow {
                authenticated_only: false,
                cluster_only: true,
            } => "cluster-unauthenticated".fmt(f),
            Self::Deny => "deny".fmt(f),
        }
    }
}

// === impl DefaultPolicyWatches ===

impl DefaultPolicyWatches {
    /// Create default allow policy receivers.
    ///
    /// These receivers are never updated. The senders are moved into a background task so that
    /// the receivers continue to be live. The returned background task completes once all receivers
    /// are dropped.
    pub(crate) fn new(cluster_nets: Vec<IpNet>, detect_timeout: time::Duration) -> Self {
        Self {
            cluster_nets,
            detect_timeout,
            watches: HashMap::default(),
        }
    }

    pub fn get(&mut self, default: DefaultPolicy, config: PortDefaults) -> ServerRx {
        use std::collections::hash_map::Entry;
        match self.watches.entry((default, config)) {
            Entry::Occupied(entry) => entry.get().1.clone(),
            Entry::Vacant(entry) => {
                let server =
                    Self::mk_server(default, config, &self.cluster_nets, self.detect_timeout);
                let (tx, rx) = watch::channel(server);
                entry.insert((tx, rx.clone()));
                rx
            }
        }
    }

    fn mk_server(
        default: DefaultPolicy,
        config: PortDefaults,
        cluster_nets: &[IpNet],
        detect_timeout: time::Duration,
    ) -> InboundServer {
        let protocol = Self::mk_protocol(detect_timeout, &config);

        match default {
            DefaultPolicy::Allow {
                cluster_only,
                authenticated_only,
            } => {
                let name = format!("default:{}", default);
                let nets = if cluster_only {
                    cluster_nets.to_vec()
                } else {
                    vec![IpNet::V4(Default::default()), IpNet::V6(Default::default())]
                };
                let authn = if authenticated_only || config.authenticated {
                    let all_authed = IdentityMatch::Suffix(vec![]);
                    ClientAuthentication::TlsAuthenticated(vec![all_authed])
                } else {
                    ClientAuthentication::Unauthenticated
                };
                Self::mk_policy(name, protocol, nets, authn)
            }

            DefaultPolicy::Deny => InboundServer {
                protocol,
                authorizations: Default::default(),
            },
        }
    }

    fn mk_protocol(timeout: time::Duration, config: &PortDefaults) -> ProxyProtocol {
        if config.opaque {
            return ProxyProtocol::Opaque;
        }
        ProxyProtocol::Detect { timeout }
    }

    fn mk_policy(
        name: String,
        protocol: ProxyProtocol,
        nets: impl IntoIterator<Item = IpNet>,
        authentication: ClientAuthentication,
    ) -> InboundServer {
        let networks = nets
            .into_iter()
            .map(|net| NetworkMatch {
                net,
                except: vec![],
            })
            .collect::<Vec<_>>();
        let authz = ClientAuthorization {
            networks,
            authentication,
        };

        InboundServer {
            protocol,
            authorizations: Some((name, authz)).into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_parse_displayed() {
        for default in [
            DefaultPolicy::Deny,
            DefaultPolicy::Allow {
                authenticated_only: true,
                cluster_only: false,
            },
            DefaultPolicy::Allow {
                authenticated_only: false,
                cluster_only: false,
            },
            DefaultPolicy::Allow {
                authenticated_only: false,
                cluster_only: true,
            },
            DefaultPolicy::Allow {
                authenticated_only: false,
                cluster_only: true,
            },
        ] {
            assert_eq!(
                default.to_string().parse::<DefaultPolicy>().unwrap(),
                default,
                "failed to parse displayed {:?}",
                default
            );
        }
    }
}
