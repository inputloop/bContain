// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Networks to put behind a guest's network card.
//!
//! [`user_nat`] gives a guest the internet through the host's ordinary
//! sockets: nothing to install, no administrator rights, no host network
//! configuration. The guest sits on a private 10.0.2.0/24 network of its
//! own; see [`nat`].

pub mod nat;
pub mod packet;

use crate::virtio::net::Net;

/// A network card with address `mac` on its own private network, with the
/// host's internet access behind it.
pub fn user_nat(mac: [u8; 6]) -> Net {
    user_nat_with(mac, nat::Options::default())
}

/// The same, with choices: see [`nat::Options`].
pub fn user_nat_with(mac: [u8; 6], options: nat::Options) -> Net {
    let (handle, runner) = nat::Nat::new(options);
    let (net, sender) = Net::new(mac, move |frame| handle.frame(frame));
    let alive = sender.clone();
    runner.start(move |frame| sender.send(frame), move || alive.is_alive());
    net
}
