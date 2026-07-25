//! The network policy: internet yes, the developer's machine and LAN no.
//!
//! `greenlit-v0-spec.md` ("Security model"): "Actions can pull dependencies
//! but can't reach `localhost`, your homelab, or anything on your subnet."
//! `PHASE-4-environment.md` adds "with exactly the required authenticated
//! pinholes for the `greenlit-store`/provisioning control boundary".
//!
//! # Why not `DOCKER-USER`
//!
//! `PHASE-4-environment.md` names host `DOCKER-USER` rules. That chain lives
//! on the host and Docker exposes no API for it, so following the letter
//! would mean shelling out to `iptables` under `sudo` on **every** `litci
//! run` — breaking the spec's "zero prerequisites" promise and the
//! never-shell-out rule. `AGENTS.md` resolves the conflict in favour of the
//! spec for product behavior, so the *intent* is met a different way.
//!
//! # The mechanism
//!
//! A short-lived sidecar is started inside the target container's own network
//! namespace (`--network container:<id>`) with `CAP_NET_ADMIN`. It installs
//! the filter rules there and exits. The rules live in the namespace, so they
//! outlive the sidecar; the capability does not.
//!
//! This is *stronger* than host-side `DOCKER-USER`, not merely equivalent:
//! the workflow container never holds `NET_ADMIN`, so code running inside it
//! cannot remove the rules it is bound by even as root with `iptables`
//! installed. Verified: an in-container `iptables -F` fails with "Permission
//! denied (you must be root)" and the drop stays in force.
//!
//! # The rules, in order
//!
//! Order is the whole design — the first match wins, so each rule carves an
//! exception out of the one below it:
//!
//! 1. **loopback** — the container's own loopback, which is not the host's.
//! 2. **established/related** — replies to connections the container opened.
//! 3. **the shim, exactly** — the bridge gateway on the one port the shim
//!    bound. This is the "authenticated pinhole": the shim still requires the
//!    run's bearer token, so reachability alone grants nothing.
//! 4. **the gateway, otherwise: dropped** — everything else on the host's
//!    bridge address, so a host service that happens to be listening there is
//!    not reachable just because the shim is.
//! 5. **this run's own subnet** — service containers, which the job is
//!    entitled to reach by service id.
//! 6. **every other private range: dropped** — RFC1918 and link-local,
//!    including the cloud metadata address, which is where "your homelab" and
//!    "your subnet" actually live.
//! 7. **everything else: allowed** — the public internet.

use crate::engine::{ContainerEngine, ContainerSpec};
use crate::error::RuntimeError;

/// The image the sidecar runs.
///
/// Kubernetes' immutable Debian iptables image carries both legacy and nft
/// backends. Using the tool-bearing image directly avoids an `apk add` network
/// download in every disposable sidecar.
pub(crate) const NETGUARD_IMAGE: &str = "registry.k8s.io/debian-iptables@sha256:852d3c569932059bcab3a52cb6105c432d85b4b7bbd5fc93153b78010e34a783";

/// Private ranges a workflow container must not reach.
///
/// `169.254.0.0/16` covers link-local *and* the cloud metadata endpoint at
/// `169.254.169.254`, which is a credential-exfiltration path on any hosted
/// machine someone runs Greenlit on.
const PRIVATE_RANGES: [&str; 4] = [
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
];

/// What the policy needs to know about this run's own network.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// The bridge gateway, which is also where the shim listens.
    pub gateway: Option<String>,
    /// The bridge subnet, whose other members are this run's services.
    pub subnet: Option<String>,
    /// The port the shim bound, if one is running.
    pub shim_port: Option<u16>,
    /// Whether destinations outside the run subnet may be reached.
    pub allow_external: bool,
}

/// Applies the policy inside `container`'s network namespace.
///
/// # Errors
/// Returns [`RuntimeError`] if the sidecar cannot be created, started, or run.
/// A policy that cannot be applied is a hard failure: `AGENTS.md` states the
/// containment invariants are "not configurable off", so a run that could not
/// be contained must not proceed as though it had been.
pub async fn apply(
    engine: &dyn ContainerEngine,
    container: &str,
    policy: &Policy,
    image: &str,
    run_id: &str,
    progress: &mut (dyn crate::progress::ProgressSink + Send),
) -> Result<(), RuntimeError> {
    if !engine.image_exists(image).await? {
        engine.pull_image(image, None, progress).await?;
    }

    let spec = ContainerSpec {
        image: image.to_string(),
        // Joining the target's namespace is what makes rules installed here
        // apply to it.
        network: Some(format!("container:{container}")),
        cap_add: vec!["NET_ADMIN".to_string()],
        entrypoint: vec!["/bin/sh".to_string(), "-c".to_string()],
        cmd: vec![script(policy)],
        labels: vec![
            ("greenlit.managed".to_string(), "1".to_string()),
            ("greenlit.run".to_string(), run_id.to_string()),
            ("greenlit.netguard".to_string(), "1".to_string()),
        ],
        ..ContainerSpec::default()
    };

    let sidecar = engine.create_container(&spec).await?;
    let outcome = engine
        .run_container(&sidecar, &mut crate::engine::SinkNull)
        .await;
    // The sidecar is disposable either way; its rules persist in the
    // namespace once installed.
    let _ = engine.remove_container(&sidecar).await;

    let output = outcome?;
    if output.exit_code == 0 {
        Ok(())
    } else {
        Err(RuntimeError::UnsupportedDockerHost {
            value: format!("network policy exited {}", output.exit_code),
            fix: "Greenlit could not contain the workflow's network access, and will not run \
                  a workflow it cannot contain. Check that the host kernel provides iptables \
                  (nf_tables or legacy) and that the daemon can grant NET_ADMIN"
                .to_string(),
        })
    }
}

/// The shell script the sidecar runs.
///
/// Emitted as text rather than built with a library because it has to run
/// inside the sidecar, where the only available tool is `iptables` itself.
fn script(policy: &Policy) -> String {
    let mut lines = vec![
        "set -e".to_string(),
        // 1 and 2: the container's own loopback and its own replies.
        "iptables -A OUTPUT -o lo -j ACCEPT".to_string(),
        "iptables -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT".to_string(),
    ];

    if let Some(gateway) = &policy.gateway {
        // 3: the shim, on exactly its port.
        if let Some(port) = policy.shim_port {
            lines.push(format!(
                "iptables -A OUTPUT -d {gateway}/32 -p tcp --dport {port} -j ACCEPT"
            ));
        }
        // 4: nothing else on the host's bridge address.
        lines.push(format!("iptables -A OUTPUT -d {gateway}/32 -j DROP"));
    }

    // 5: this run's own services.
    if let Some(subnet) = &policy.subnet {
        lines.push(format!("iptables -A OUTPUT -d {subnet} -j ACCEPT"));
    }

    // 6: every other private range.
    for range in PRIVATE_RANGES {
        lines.push(format!("iptables -A OUTPUT -d {range} -j DROP"));
    }
    if !policy.allow_external {
        lines.push("iptables -A OUTPUT -j REJECT".to_string());
    }

    // 7 is the absence of a rule: the policy is ACCEPT, so the public
    // internet stays reachable.
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{Policy, script};

    fn rules() -> Vec<String> {
        script(&Policy {
            gateway: Some("172.18.0.1".to_string()),
            subnet: Some("172.18.0.0/16".to_string()),
            shim_port: Some(43111),
            allow_external: true,
        })
        .lines()
        .map(str::to_string)
        .collect()
    }

    /// The order *is* the policy: first match wins, so an exception has to
    /// precede the rule it carves out of.
    #[test]
    fn the_shim_pinhole_precedes_the_gateway_drop() {
        let rules = rules();
        let pinhole = rules
            .iter()
            .position(|rule| rule.contains("--dport 43111"))
            .expect("the shim is reachable");
        let gateway_drop = rules
            .iter()
            .position(|rule| rule.contains("-d 172.18.0.1/32 -j DROP"))
            .expect("the rest of the gateway is not");
        assert!(
            pinhole < gateway_drop,
            "a pinhole after the drop would never match, silently cutting the cache off"
        );
    }

    #[test]
    fn this_runs_subnet_precedes_the_private_range_drops() {
        let rules = rules();
        let subnet = rules
            .iter()
            .position(|rule| rule.contains("-d 172.18.0.0/16 -j ACCEPT"))
            .expect("services are reachable");
        let private = rules
            .iter()
            .position(|rule| rule.contains("172.16.0.0/12"))
            .expect("other private space is not");
        assert!(
            subnet < private,
            "the run's own bridge sits inside RFC1918, so its accept must come first \
             or every service becomes unreachable"
        );
    }

    #[test]
    fn every_private_range_including_metadata_is_dropped() {
        let joined = rules().join("\n");
        for range in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
            assert!(joined.contains(range), "{range} must be dropped");
        }
        assert!(
            joined.contains("169.254.0.0/16"),
            "link-local covers the cloud metadata endpoint, a credential path"
        );
    }

    #[test]
    fn replies_and_own_loopback_are_allowed_first() {
        let rules = rules();
        assert!(rules[1].contains("-o lo -j ACCEPT"));
        assert!(rules[2].contains("ESTABLISHED,RELATED"));
    }

    #[test]
    fn a_run_with_no_shim_still_drops_the_gateway() {
        let rules = script(&Policy {
            gateway: Some("172.18.0.1".to_string()),
            subnet: Some("172.18.0.0/16".to_string()),
            shim_port: None,
            allow_external: true,
        });
        assert!(!rules.contains("--dport"), "no shim, no pinhole");
        assert!(
            rules.contains("-d 172.18.0.1/32 -j DROP"),
            "the host is still unreachable"
        );
    }

    #[test]
    fn hermetic_policy_rejects_every_destination_after_internal_exceptions() {
        let rules = script(&Policy {
            gateway: Some("172.18.0.1".to_string()),
            subnet: Some("172.18.0.0/16".to_string()),
            shim_port: Some(43111),
            allow_external: false,
        });
        assert_eq!(
            rules.lines().last(),
            Some("iptables -A OUTPUT -j REJECT"),
            "hermetic policy must fail closed after loopback, reply, shim, and service rules"
        );
    }
}
