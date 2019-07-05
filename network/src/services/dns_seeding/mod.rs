use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ckb_logger::{debug, error, info, trace, warn};
use faster_hex::hex_decode;
use futures::{Async, Future, Poll, Stream};
use p2p::{multiaddr::Protocol, secio::PeerId};
use resolve::record::Txt;
use resolve::{DnsConfig, DnsResolver};
use secp256k1::key::PublicKey;
use tokio::timer::Interval;

mod seed_record;

use crate::NetworkState;
use seed_record::SeedRecord;

// FIXME: should replace this later
const TXT_VERIFY_PUBKEY: &str = "";

pub(crate) struct DnsSeedingService {
    network_state: Arc<NetworkState>,
    wait_until: Instant,
    // Because tokio timer is not reliable
    check_interval: Interval,
    seeds: Vec<String>,
}

impl DnsSeedingService {
    pub(crate) fn new(network_state: Arc<NetworkState>, seeds: Vec<String>) -> DnsSeedingService {
        let wait_until =
            if network_state.with_peer_store(|peer_store| peer_store.random_peers(1).is_empty()) {
                info!("No peer in peer store, start seeding...");
                Instant::now()
            } else {
                Instant::now() + Duration::from_secs(11)
            };
        let check_interval = Interval::new_interval(Duration::from_secs(1));
        DnsSeedingService {
            network_state,
            wait_until,
            check_interval,
            seeds,
        }
    }

    fn seeding(&self) -> Result<(), Box<dyn Error>> {
        // TODO: DNS seeding is disabled now, may enbale in the future (need discussed)
        if TXT_VERIFY_PUBKEY.is_empty() {
            return Ok(());
        }

        let enough_outbound = self.network_state.with_peer_registry(|reg| {
            reg.peers()
                .values()
                .filter(|peer| peer.is_outbound())
                .count()
                >= 2
        });
        if enough_outbound {
            debug!("Enough outbound peers");
            return Ok(());
        }

        let mut pubkey_bytes = [4u8; 65];
        hex_decode(TXT_VERIFY_PUBKEY.as_bytes(), &mut pubkey_bytes[1..65])
            .map_err(|err| format!("parse key({}) error: {:?}", TXT_VERIFY_PUBKEY, err))?;
        let pubkey = PublicKey::from_slice(&pubkey_bytes)
            .map_err(|err| format!("create PublicKey failed: {:?}", err))?;

        let resolver = DnsConfig::load_default()
            .map_err(|err| format!("Failed to load system configuration: {}", err))
            .and_then(|config| {
                DnsResolver::new(config)
                    .map_err(|err| format!("Failed to create DNS resolver: {}", err))
            })?;

        let mut addrs = Vec::new();
        for seed in &self.seeds {
            debug!("query txt records from: {}", seed);
            match resolver.resolve_record::<Txt>(seed) {
                Ok(records) => {
                    for record in records {
                        match std::str::from_utf8(&record.data) {
                            Ok(record) => match SeedRecord::decode_with_pubkey(&record, &pubkey) {
                                Ok(seed_record) => {
                                    let address = seed_record.address();
                                    trace!("got dns txt address: {}", address);
                                    addrs.push(address);
                                }
                                Err(err) => {
                                    debug!("decode dns txt record failed: {:?}, {:?}", err, record);
                                }
                            },
                            Err(err) => {
                                debug!("get dns txt record error: {:?}", err);
                            }
                        }
                    }
                }
                Err(_) => {
                    warn!("Invalid domain name: {}", seed);
                }
            }
        }

        debug!("DNS seeding got {} address", addrs.len());
        self.network_state.with_peer_store_mut(|peer_store| {
            for mut addr in addrs {
                match addr.pop() {
                    Some(Protocol::P2p(key)) => {
                        if let Ok(peer_id) = PeerId::from_bytes(key.into_bytes()) {
                            peer_store.add_discovered_addr(&peer_id, addr);
                        }
                    }
                    _ => {
                        debug!("Got addr without peer_id: {}, ignore it", addr);
                    }
                }
            }
        });
        Ok(())
    }
}

impl Future for DnsSeedingService {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.check_interval.poll() {
                Ok(Async::Ready(Some(_))) => {
                    if self.wait_until < Instant::now() {
                        if let Err(err) = self.seeding() {
                            error!("seeding error: {:?}", err);
                        }
                        debug!("DNS seeding finished");
                        return Ok(Async::Ready(()));
                    } else {
                        trace!("DNS check interval");
                    }
                }
                Ok(Async::Ready(None)) => {
                    warn!("Poll DnsSeedingService interval return None");
                    return Err(());
                }
                Ok(Async::NotReady) => break,
                Err(err) => {
                    warn!("Poll DnsSeedingService interval error: {:?}", err);
                    return Err(());
                }
            }
        }
        Ok(Async::NotReady)
    }
}
