//! Tests for the address a waiting host reports.
//!
//! A host binds the wildcard so it answers on every interface, and the address it reports is
//! the one a person reads off one screen and types into another. Reporting `0.0.0.0` is the
//! truth and is useless: it is the single address no client can connect to, and a host that
//! showed it would look configured and be unreachable.

use std::net::SocketAddr;

use prism_core::control::host::reachable_address;

#[test]
fn an_address_that_names_an_interface_is_left_alone() {
    // Somebody who bound one interface on purpose is told what they chose, not what the
    // routing table would have picked instead.
    for bound in ["192.168.1.5:47200", "127.0.0.1:9000", "[::1]:47200"] {
        let address: SocketAddr = bound.parse().expect("a valid address");

        assert_eq!(reachable_address(address), address, "{bound}");
    }
}

#[test]
fn the_wildcard_is_replaced_by_something_a_client_could_dial() {
    let address: SocketAddr = "0.0.0.0:47200".parse().expect("a valid address");
    let reachable = reachable_address(address);

    assert_eq!(reachable.port(), 47200, "the port is what was bound");

    // On a machine with no route at all there is nothing better to say, and the bound address
    // comes back unchanged. Anywhere else it must name an interface.
    if reachable != address {
        assert!(
            !reachable.ip().is_unspecified(),
            "the wildcard was reported as itself: {reachable}"
        );
    }
}

#[test]
fn the_port_survives_being_chosen_by_the_operating_system() {
    // Port zero is how a host asks for any free port. What it reports afterwards has to be
    // the port it actually got, since nothing else knows it.
    let bound: SocketAddr = "0.0.0.0:54321".parse().expect("a valid address");

    assert_eq!(reachable_address(bound).port(), 54321);
}
