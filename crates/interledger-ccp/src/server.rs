#[cfg(test)]
use crate::packet::PEER_PROTOCOL_CONDITION;
use crate::{
    packet::{
        Mode, Route, RouteControlRequest, RouteUpdateRequest, CCP_CONTROL_DESTINATION,
        CCP_RESPONSE, CCP_UPDATE_DESTINATION,
    },
    routing_table::RoutingTable,
    CcpRoutingAccount, RouteManagerStore, RoutingRelation,
};
use bytes::Bytes;
use futures::{
    future::{err, join_all, ok, Either},
    Future, Stream,
};
#[cfg(test)]
use interledger_packet::PrepareBuilder;
use interledger_packet::{Address, ErrorCode, Fulfill, Reject, RejectBuilder};
use interledger_service::{
    Account, AddressStore, BoxedIlpFuture, IncomingRequest, IncomingService, OutgoingRequest,
    OutgoingService,
};
#[cfg(test)]
use lazy_static::lazy_static;
use log::{debug, error, trace, warn};
use parking_lot::{Mutex, RwLock};
use ring::digest::{digest, SHA256};
use std::collections::HashMap;
use std::{
    cmp::min,
    convert::TryFrom,
    str,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio_timer::Interval;

#[cfg(not(test))]
use tokio_executor::spawn;

// TODO should the route expiry be longer? we use 30 seconds now
// because the expiry shortener will lower the expiry to 30 seconds
// otherwise. we could make it longer and make sure the BTP server
// comes after the expiry shortener
const DEFAULT_ROUTE_EXPIRY_TIME: u32 = 30000;
const DEFAULT_BROADCAST_INTERVAL: u64 = 30000;
const DUMMY_ROUTING_TABLE_ID: [u8; 16] = [0; 16];

fn hash(preimage: &[u8; 32]) -> [u8; 32] {
    let mut out = [0; 32];
    out.copy_from_slice(digest(&SHA256, preimage).as_ref());
    out
}

type NewAndWithdrawnRoutes = (Vec<Route>, Vec<Bytes>);

pub struct CcpRouteManagerBuilder<I, O, S> {
    /// The next request handler that will be used both to pass on requests that are not CCP messages.
    next_incoming: I,
    /// The outgoing request handler that will be used to send outgoing CCP messages.
    /// Note that this service bypasses the Router because the Route Manager needs to be able to
    /// send messages directly to specific peers.
    outgoing: O,
    /// This represents the routing table we will forward to our peers.
    /// It is the same as the local_table with our own address added to the path of each route.
    store: S,
    ilp_address: Address,
    broadcast_interval: u64,
}

impl<I, O, S, A> CcpRouteManagerBuilder<I, O, S>
where
    I: IncomingService<A> + Clone + Send + Sync + 'static,
    O: OutgoingService<A> + Clone + Send + Sync + 'static,
    S: AddressStore + RouteManagerStore<Account = A> + Clone + Send + Sync + 'static,
    A: CcpRoutingAccount + Send + Sync + 'static,
{
    pub fn new(ilp_address: Address, store: S, outgoing: O, next_incoming: I) -> Self {
        CcpRouteManagerBuilder {
            ilp_address,
            next_incoming,
            outgoing,
            store,
            broadcast_interval: DEFAULT_BROADCAST_INTERVAL,
        }
    }

    pub fn ilp_address(&mut self, ilp_address: Address) -> &mut Self {
        self.ilp_address = ilp_address;
        self
    }

    /// Set the broadcast interval (in milliseconds)
    pub fn broadcast_interval(&mut self, ms: u64) -> &mut Self {
        self.broadcast_interval = ms;
        self
    }

    pub fn to_service(&self) -> CcpRouteManager<I, O, S, A> {
        #[allow(clippy::let_and_return)]
        let service = CcpRouteManager {
            ilp_address: Arc::new(RwLock::new(self.ilp_address.clone())),
            next_incoming: self.next_incoming.clone(),
            outgoing: self.outgoing.clone(),
            store: self.store.clone(),
            forwarding_table: Arc::new(RwLock::new(RoutingTable::default())),
            forwarding_table_updates: Arc::new(RwLock::new(Vec::new())),
            last_epoch_updates_sent_for: Arc::new(AtomicU32::new(0)),
            local_table: Arc::new(RwLock::new(RoutingTable::default())),
            incoming_tables: Arc::new(RwLock::new(HashMap::new())),
            unavailable_accounts: Arc::new(Mutex::new(HashMap::new())),
        };

        #[cfg(not(test))]
        {
            spawn(service.start_broadcast_interval(self.broadcast_interval));
        }

        service
    }
}

#[derive(Debug)]
struct BackoffParams {
    /// The total number of route broadcast intervals we should wait before trying again
    /// This is incremented for each broadcast failure
    max: u8,
    /// How many more intervals we should wait before trying to send again
    /// (0 means we should try again on the next loop)
    skip_intervals: u8,
}

/// The Routing Manager Service.
///
/// This implements the Connector-to-Connector Protocol (CCP)
/// for exchanging route updates with peers. This service handles incoming CCP messages
/// and sends updates to peers. It manages the routing table in the Store and updates it
/// with the best routes determined by per-account configuration and the broadcasts we have
/// received from peers.
#[derive(Clone)]
pub struct CcpRouteManager<I, O, S, A: Account> {
    ilp_address: Arc<RwLock<Address>>,
    /// The next request handler that will be used both to pass on requests that are not CCP messages.
    next_incoming: I,
    /// The outgoing request handler that will be used to send outgoing CCP messages.
    /// Note that this service bypasses the Router because the Route Manager needs to be able to
    /// send messages directly to specific peers.
    outgoing: O,
    /// This represents the routing table we will forward to our peers.
    /// It is the same as the local_table with our own address added to the path of each route.
    forwarding_table: Arc<RwLock<RoutingTable<A>>>,
    last_epoch_updates_sent_for: Arc<AtomicU32>,
    /// These updates are stored such that index 0 is the transition from epoch 0 to epoch 1
    forwarding_table_updates: Arc<RwLock<Vec<NewAndWithdrawnRoutes>>>,
    /// This is the routing table we have compile from configuration and
    /// broadcasts we have received from our peers. It is saved to the Store so that
    /// the Router services forwards packets according to what it says.
    local_table: Arc<RwLock<RoutingTable<A>>>,
    /// We store a routing table for each peer we receive Route Update Requests from.
    /// When the peer sends us an update, we apply that update to this view of their table.
    /// Updates from peers are applied to our local_table if they are better than the
    /// existing best route and if they do not attempt to overwrite configured routes.
    incoming_tables: Arc<RwLock<HashMap<A::AccountId, RoutingTable<A>>>>,
    store: S,
    /// If we get final errors while sending to specific accounts, we'll
    /// wait before trying to broadcast to them
    /// This maps the account ID to the number of route brodcast intervals
    /// we should wait before trying again
    unavailable_accounts: Arc<Mutex<HashMap<A::AccountId, BackoffParams>>>,
}

impl<I, O, S, A> CcpRouteManager<I, O, S, A>
where
    I: IncomingService<A> + Clone + Send + Sync + 'static,
    O: OutgoingService<A> + Clone + Send + Sync + 'static,
    S: AddressStore + RouteManagerStore<Account = A> + Clone + Send + Sync + 'static,
    A: CcpRoutingAccount + Send + Sync + 'static,
{
    /// Returns a future that will trigger this service to update its routes and broadcast
    /// updates to peers on the given interval.
    pub fn start_broadcast_interval(&self, interval: u64) -> impl Future<Item = (), Error = ()> {
        let clone = self.clone();
        self.request_all_routes().and_then(move |_| {
            Interval::new(Instant::now(), Duration::from_millis(interval))
                .map_err(|err| error!("Interval error, no longer sending route updates: {:?}", err))
                .for_each(move |_| {
                    // ensure we have the latest ILP Address from the store
                    clone.update_ilp_address();
                    // Returning an error would end the broadcast loop
                    // so we want to return Ok even if there was an error
                    clone.broadcast_routes().then(|_| Ok(()))
                })
        })
    }

    fn update_ilp_address(&self) {
        let current_ilp_address = self.ilp_address.read();
        let ilp_address = self.store.get_ilp_address();
        if ilp_address != *current_ilp_address {
            debug!(
                "Changing ILP address from {} to {}",
                *current_ilp_address, ilp_address
            );
            // release the read lock
            drop(current_ilp_address);
            *self.ilp_address.write() = ilp_address;
        }
    }

    pub fn broadcast_routes(&self) -> impl Future<Item = (), Error = ()> {
        let clone = self.clone();
        self.update_best_routes(None)
            .and_then(move |_| clone.send_route_updates())
    }

    /// Request routes from all the peers we are willing to receive routes from.
    /// This is mostly intended for when the CCP server starts up and doesn't have any routes from peers.
    fn request_all_routes(&self) -> impl Future<Item = (), Error = ()> {
        let clone = self.clone();
        self.store
            .get_accounts_to_receive_routes_from()
            .then(|result| {
                let accounts = result.unwrap_or_else(|_| Vec::new());
                join_all(accounts.into_iter().map(move |account| {
                    clone.send_route_control_request(account, DUMMY_ROUTING_TABLE_ID, 0)
                }))
            })
            .then(|_| Ok(()))
    }

    /// Handle a CCP Route Control Request. If this is from an account that we broadcast routes to,
    /// we'll send an outgoing Route Update Request to them.
    fn handle_route_control_request(
        &self,
        request: IncomingRequest<A>,
    ) -> impl Future<Item = Fulfill, Error = Reject> {
        if !request.from.should_send_routes() {
            return Either::A(err(RejectBuilder {
                code: ErrorCode::F00_BAD_REQUEST,
                message: b"We are not configured to send routes to you, sorry",
                triggered_by: Some(&self.ilp_address.read()),
                data: &[],
            }
            .build()));
        }

        let control = RouteControlRequest::try_from(&request.prepare);
        if control.is_err() {
            return Either::A(err(RejectBuilder {
                code: ErrorCode::F00_BAD_REQUEST,
                message: b"Invalid route control request",
                triggered_by: Some(&self.ilp_address.read()),
                data: &[],
            }
            .build()));
        }
        let control = control.unwrap();
        debug!(
            "Got route control request from account {} (id: {}): {:?}",
            request.from.username(),
            request.from.id(),
            control
        );

        // TODO stop sending updates if they are in Idle mode
        if control.mode == Mode::Sync {
            // Don't skip them in the route update broadcasts anymore since this
            // tells us that they are online
            // TODO what happens if they can send to us but we can't send to them?
            {
                trace!("Checking whether account was previously listed as unavailable");
                let mut unavailable_accounts = self.unavailable_accounts.lock();
                if unavailable_accounts.remove(&request.from.id()).is_some() {
                    debug!("Account {} (id: {}) is no longer unavailable, will resume broadcasting routes to it",
                            request.from.username(),
                            request.from.id());
                }
            }

            let (from_epoch_index, to_epoch_index) = {
                let forwarding_table = self.forwarding_table.read();
                let to_epoch_index = forwarding_table.epoch();
                let from_epoch_index =
                    if control.last_known_routing_table_id != forwarding_table.id() {
                        0
                    } else {
                        min(control.last_known_epoch, to_epoch_index)
                    };
                (from_epoch_index, to_epoch_index)
            };

            #[cfg(test)]
            {
                let ilp_address = self.ilp_address.read().clone();
                return Either::B(Either::A(
                    self.send_route_update(request.from.clone(), from_epoch_index, to_epoch_index)
                        .map_err(move |_| {
                            RejectBuilder {
                                code: ErrorCode::T01_PEER_UNREACHABLE,
                                message: b"Error sending route update request",
                                data: &[],
                                triggered_by: Some(&ilp_address),
                            }
                            .build()
                        })
                        .and_then(|_| Ok(CCP_RESPONSE.clone())),
                ));
            }

            #[cfg(not(test))]
            {
                spawn(self.send_route_update(
                    request.from.clone(),
                    from_epoch_index,
                    to_epoch_index,
                ));
            }
        }

        #[cfg(not(test))]
        {
            Either::B(ok(CCP_RESPONSE.clone()))
        }

        #[cfg(test)]
        {
            Either::B(Either::B(ok(CCP_RESPONSE.clone())))
        }
    }

    /// Remove invalid routes before processing the Route Update Request
    fn filter_routes(&self, mut update: RouteUpdateRequest) -> RouteUpdateRequest {
        update.new_routes = update
            .new_routes
            .into_iter()
            .filter(|route| {
                let ilp_address = self.ilp_address.read();
                let address_scheme = (*ilp_address).scheme().as_bytes();
                if !route.prefix.starts_with(address_scheme) {
                    warn!("Got route for a different global prefix: {:?}", route);
                    false
                } else if route.prefix.len() <= address_scheme.len() + 1 {
                    // note the + 1 is due to address_scheme not including a trailing "."
                    warn!("Got route broadcast for the global prefix: {:?}", route);
                    false
                } else if route.prefix.starts_with(self.ilp_address.read().as_ref()) {
                    trace!("Ignoring route broadcast for a prefix that starts with our own address: {:?}", route);
                    false
                } else if route.path.contains(self.ilp_address.read().as_ref()) {
                    trace!(
                        "Ignoring route broadcast for a route that includes us: {:?}",
                        route
                    );
                    false
                } else {
                    true
                }
            })
            .collect();
        update
    }

    /// Check if this Route Update Request is valid and, if so, apply any updates it contains.
    /// If updates are applied to the Incoming Routing Table for this peer, we will
    /// then check whether those routes are better than the current best ones we have in the
    /// Local Routing Table.
    fn handle_route_update_request(&self, request: IncomingRequest<A>) -> BoxedIlpFuture {
        // Ignore the request if we don't accept routes from them
        if !request.from.should_receive_routes() {
            return Box::new(err(RejectBuilder {
                code: ErrorCode::F00_BAD_REQUEST,
                message: b"Your route broadcasts are not accepted here",
                triggered_by: Some(&self.ilp_address.read()),
                data: &[],
            }
            .build()));
        }

        let update = RouteUpdateRequest::try_from(&request.prepare);
        if update.is_err() {
            return Box::new(err(RejectBuilder {
                code: ErrorCode::F00_BAD_REQUEST,
                message: b"Invalid route update request",
                triggered_by: Some(&self.ilp_address.read()),
                data: &[],
            }
            .build()));
        }
        let update = update.unwrap();
        debug!(
            "Got route update request from account {}: {:?}",
            request.from.id(),
            update
        );

        // Filter out routes that don't make sense or that we won't accept
        let update = self.filter_routes(update);

        let mut incoming_tables = self.incoming_tables.write();
        if !&incoming_tables.contains_key(&request.from.id()) {
            incoming_tables.insert(
                request.from.id(),
                RoutingTable::new(update.routing_table_id),
            );
        }

        // Update the routing table we maintain for the account we got this from.
        // Figure out whether we need to update our routes for any of the prefixes
        // that were included in this route update.
        match (*incoming_tables)
            .get_mut(&request.from.id())
            .expect("Should have inserted a routing table for this account")
            .handle_update_request(request.from.clone(), update)
        {
            Ok(prefixes_updated) => {
                if prefixes_updated.is_empty() {
                    trace!("Route update request did not contain any prefixes we need to update our routes for");
                    return Box::new(ok(CCP_RESPONSE.clone()));
                }

                debug!("Recalculating best routes for prefixes: {}", {
                    let updated: Vec<&str> = prefixes_updated
                        .iter()
                        .map(|prefix| str::from_utf8(&prefix).unwrap_or("<not utf8>"))
                        .collect();
                    updated.join(", ")
                });
                let future = self.update_best_routes(Some(prefixes_updated));

                #[cfg(not(test))]
                {
                    spawn(future);
                    Box::new(ok(CCP_RESPONSE.clone()))
                }

                #[cfg(test)]
                {
                    let ilp_address = self.ilp_address.clone();
                    Box::new(
                        future
                            .map_err(move |_| {
                                RejectBuilder {
                                    code: ErrorCode::T00_INTERNAL_ERROR,
                                    message: b"Error processing route update",
                                    data: &[],
                                    triggered_by: Some(&ilp_address.read()),
                                }
                                .build()
                            })
                            .and_then(|_| Ok(CCP_RESPONSE.clone())),
                    )
                }
            }
            Err(message) => {
                warn!("Error handling incoming Route Update request, sending a Route Control request to get updated routing table info from peer. Error was: {}", &message);
                let reject = RejectBuilder {
                    code: ErrorCode::F00_BAD_REQUEST,
                    message: &message.as_bytes(),
                    data: &[],
                    triggered_by: Some(&self.ilp_address.read()),
                }
                .build();
                let table = &incoming_tables[&request.from.id()];
                let future = self.send_route_control_request(
                    request.from.clone(),
                    table.id(),
                    table.epoch(),
                );
                #[cfg(not(test))]
                {
                    spawn(future);
                    Box::new(err(reject))
                }
                #[cfg(test)]
                Box::new(future.then(move |_| Err(reject)))
            }
        }
    }

    /// Request a Route Update from the specified peer. This is sent when we get
    /// a Route Update Request from them with a gap in the epochs since the last one we saw.
    fn send_route_control_request(
        &self,
        account: A,
        last_known_routing_table_id: [u8; 16],
        last_known_epoch: u32,
    ) -> impl Future<Item = (), Error = ()> {
        let account_id = account.id();
        let control = RouteControlRequest {
            mode: Mode::Sync,
            last_known_routing_table_id,
            last_known_epoch,
            features: Vec::new(),
        };
        debug!("Sending Route Control Request to account: {} (id: {}), last known table id: {}, last known epoch: {}",
            account.username(),
            account_id,
            hex::encode(&last_known_routing_table_id[..]),
            last_known_epoch);
        let prepare = control.to_prepare();
        self.clone()
            .outgoing
            .send_request(OutgoingRequest {
                // TODO If we start charging or paying for CCP broadcasts we'll need to
                // have a separate account that we send from, but for now it's fine to
                // set the peer's account as the from account as well as the to account
                from: account.clone(),
                to: account,
                original_amount: prepare.amount(),
                prepare,
            })
            .then(move |result| {
                if let Err(err) = result {
                    warn!(
                        "Error sending Route Control Request to account {}: {:?}",
                        account_id, err
                    )
                }
                Ok(())
            })
    }

    /// Check whether the Local Routing Table currently has the best routes for the
    /// given prefixes. This is triggered when we get an incoming Route Update Request
    /// with some new or modified routes that might be better than our existing ones.
    ///
    /// If prefixes is None, this will check the best routes for all local and configured prefixes.
    fn update_best_routes(
        &self,
        prefixes: Option<Vec<Bytes>>,
    ) -> impl Future<Item = (), Error = ()> + 'static {
        let local_table = self.local_table.clone();
        let forwarding_table = self.forwarding_table.clone();
        let forwarding_table_updates = self.forwarding_table_updates.clone();
        let incoming_tables = self.incoming_tables.clone();
        let ilp_address = self.ilp_address.read().clone();
        let mut store = self.store.clone();

        self.store.get_local_and_configured_routes().and_then(
            move |(ref local_routes, ref configured_routes)| {
                let (better_routes, withdrawn_routes) = {
                    // Note we only use a read lock here and later get a write lock if we need to update the table
                    let local_table = local_table.read();
                    let incoming_tables = incoming_tables.read();

                    // Either check the given prefixes or check all of our local and configured routes
                    let prefixes_to_check: Box<dyn Iterator<Item = Bytes>> =
                        if let Some(prefixes) = prefixes {
                            Box::new(prefixes.into_iter())
                        } else {
                            let routes = configured_routes.iter().chain(local_routes.iter());
                            Box::new(routes.map(|(prefix, _account)| prefix.clone()))
                        };

                    // Check all the prefixes to see which ones we have different routes for
                    // and which ones we don't have routes for anymore
                    let mut better_routes: Vec<(Bytes, A, Route)> =
                        Vec::with_capacity(prefixes_to_check.size_hint().0);
                    let mut withdrawn_routes: Vec<Bytes> = Vec::new();
                    for prefix in prefixes_to_check {
                        // See which prefixes there is now a better route for
                        if let Some((best_next_account, best_route)) = get_best_route_for_prefix(
                            local_routes,
                            configured_routes,
                            &incoming_tables,
                            prefix.as_ref(),
                        ) {
                            if let Some((ref next_account, ref route)) =
                                local_table.get_route(&prefix)
                            {
                                if next_account.id() == best_next_account.id() {
                                    continue;
                                } else {
                                    better_routes.push((
                                        prefix.clone(),
                                        next_account.clone(),
                                        route.clone(),
                                    ));
                                }
                            } else {
                                better_routes.push((prefix.clone(), best_next_account, best_route));
                            }
                        } else {
                            // No longer have a route to this prefix
                            withdrawn_routes.push(prefix);
                        }
                    }
                    (better_routes, withdrawn_routes)
                };

                // Update the local and forwarding tables
                if !better_routes.is_empty() || !withdrawn_routes.is_empty() {
                    let mut local_table = local_table.write();
                    let mut forwarding_table = forwarding_table.write();
                    let mut forwarding_table_updates = forwarding_table_updates.write();

                    let mut new_routes: Vec<Route> = Vec::with_capacity(better_routes.len());

                    for (prefix, account, mut route) in better_routes {
                        debug!(
                            "Setting new route for prefix: {} -> Account: {} (id: {})",
                            str::from_utf8(prefix.as_ref()).unwrap_or("<not utf8>"),
                            account.username(),
                            account.id(),
                        );
                        local_table.set_route(prefix.clone(), account.clone(), route.clone());

                        // Update the forwarding table

                        // Don't advertise routes that don't start with the global prefix
                        // or that advertise the whole global prefix
                        let address_scheme = ilp_address.scheme().as_bytes();
                        let correct_address_scheme = route.prefix.starts_with(address_scheme)
                            && route.prefix != *address_scheme;
                        // We do want to advertise our address
                        let is_our_address = route.prefix == (ilp_address.as_ref() as &Bytes);
                        // Don't advertise local routes because advertising only our address
                        // will be enough to ensure the packet gets to us and we can route it
                        // to the correct account on our node
                        let is_local_route =
                            route.prefix.starts_with(ilp_address.as_ref()) && route.path.is_empty();
                        let not_local_route = is_our_address || !is_local_route;
                        // Don't include routes we're also withdrawing
                        let not_withdrawn_route = !withdrawn_routes.contains(&prefix);

                        if correct_address_scheme && not_local_route && not_withdrawn_route {
                            let old_route = forwarding_table.get_route(&prefix);
                            if old_route.is_none() || old_route.unwrap().0.id() != account.id() {
                                route.path.insert(0, ilp_address.to_bytes());
                                // Each hop hashes the auth before forwarding
                                route.auth = hash(&route.auth);
                                forwarding_table.set_route(
                                    prefix.clone(),
                                    account.clone(),
                                    route.clone(),
                                );
                                new_routes.push(route);
                            }
                        }
                    }

                    for prefix in withdrawn_routes.iter() {
                        debug!(
                            "Removed route for prefix: {}",
                            str::from_utf8(&prefix[..]).unwrap_or("<not utf8>")
                        );
                        local_table.delete_route(prefix);
                        forwarding_table.delete_route(prefix);
                    }

                    let epoch = forwarding_table.increment_epoch();
                    forwarding_table_updates.push((new_routes, withdrawn_routes));
                    debug_assert_eq!(epoch as usize + 1, forwarding_table_updates.len());

                    Either::A(store.set_routes(local_table.get_simplified_table()))
                } else {
                    // The routing table hasn't changed
                    Either::B(ok(()))
                }
            },
        )
    }

    /// Send RouteUpdateRequests to all peers that we send routing messages to
    fn send_route_updates(&self) -> impl Future<Item = (), Error = ()> {
        let self_clone = self.clone();
        let unavailable_accounts = self.unavailable_accounts.clone();
        // Check which accounts we should skip this iteration
        let accounts_to_skip: Vec<A::AccountId> = {
            trace!("Checking accounts to skip");
            let mut unavailable_accounts = self.unavailable_accounts.lock();
            let mut skip = Vec::new();
            for (id, mut backoff) in unavailable_accounts.iter_mut() {
                if backoff.skip_intervals > 0 {
                    skip.push(*id);
                }
                backoff.skip_intervals = backoff.skip_intervals.saturating_sub(1);
            }
            skip
        };
        trace!("Skipping accounts: {:?}", accounts_to_skip);
        self.store
            .get_accounts_to_send_routes_to(accounts_to_skip)
            .and_then(move |mut accounts| {
                let mut outgoing = self_clone.outgoing.clone();
                let to_epoch_index = self_clone.forwarding_table.read().epoch();
                let from_epoch_index = self_clone.last_epoch_updates_sent_for.swap(to_epoch_index, Ordering::SeqCst);

                let route_update_request =
                    self_clone.create_route_update(from_epoch_index, to_epoch_index);

                let prepare = route_update_request.to_prepare();
                accounts.sort_unstable_by_key(|a| a.id().to_string());
                accounts.dedup_by_key(|a| a.id());

                let broadcasting = !accounts.is_empty();
                if broadcasting {
                    trace!(
                        "Sending route update for epochs {} - {} to accounts: {:?} {}",
                        from_epoch_index,
                        to_epoch_index,
                        route_update_request,
                        {
                            let account_list: Vec<String> = accounts
                                .iter()
                                .map(|a| {
                                    format!(
                                        "{} (id: {}, ilp_address: {})",
                                        a.username(),
                                        a.id(),
                                        a.ilp_address()
                                    )
                                })
                                .collect();
                            account_list.join(", ")
                        }
                    );
                    Either::A(
                        join_all(accounts.into_iter().map(move |account| {
                            outgoing
                                .send_request(OutgoingRequest {
                                    from: account.clone(),
                                    to: account.clone(),
                                    original_amount: prepare.amount(),
                                    prepare: prepare.clone(),
                                })
                                .then(move |res| Ok((account, res)))
                        }))
                        .and_then(move |results: Vec<(A, Result<Fulfill, Reject>)>| {
                            // Handle the results of the route broadcast attempts
                            trace!("Updating unavailable accounts");
                            let mut unavailable_accounts = unavailable_accounts.lock();
                            for (account, result) in results.into_iter() {
                                match (account.routing_relation(), result) {
                                    (RoutingRelation::Child, Err(err)) => {
                                        if let Some(backoff) = unavailable_accounts.get_mut(&account.id()) {
                                            // Increase the number of intervals we'll skip
                                            // (but don't overflow the value it's stored in)
                                            backoff.max = backoff.max.saturating_add(1);
                                            backoff.skip_intervals = backoff.max;
                                        } else {
                                            // Skip sending to this account next time
                                            unavailable_accounts.insert(account.id(), BackoffParams {
                                                max: 1,
                                                skip_intervals: 1,
                                            });
                                        }
                                        trace!("Error sending route update to {:?} account {} (id: {}), increased backoff to {}: {:?}",
                                            account.routing_relation(), account.username(), account.id(), unavailable_accounts[&account.id()].max, err);
                                    },
                                    (_, Err(err)) => {
                                        warn!("Error sending route update to {:?} account {} (id: {}): {:?}",
                                            account.routing_relation(), account.username(), account.id(), err);
                                    },
                                    (_, Ok(_)) => {
                                        if unavailable_accounts.remove(&account.id()).is_some() {
                                            debug!("Account {} (id: {}) is no longer unavailable, resuming route broadcasts", account.username(), account.id());
                                        }
                                    }
                                }
                            }
                            Ok(())
                        }),
                    )
                } else {
                    trace!("No accounts to broadcast routes to");
                    Either::B(ok(()))
                }
            })
    }

    /// Create a RouteUpdateRequest representing the given range of Forwarding Routing Table epochs.
    /// If the epoch range is not specified, it will create an update for the last epoch only.
    fn create_route_update(
        &self,
        from_epoch_index: u32,
        to_epoch_index: u32,
    ) -> RouteUpdateRequest {
        let (start, end) = (from_epoch_index as usize, to_epoch_index as usize);
        let (routing_table_id, current_epoch_index) = {
            let table = self.forwarding_table.read();
            (table.id(), table.epoch())
        };
        let forwarding_table_updates = self.forwarding_table_updates.read();
        let epochs_to_take = end.saturating_sub(start);

        // Merge the new routes and withdrawn routes from all of the given epochs
        let mut new_routes: Vec<Route> = Vec::with_capacity(epochs_to_take);
        let mut withdrawn_routes: Vec<Bytes> = Vec::new();

        // Include our own prefix if its the first update
        // TODO this might not be the right place to send our prefix
        // (the reason we don't include our prefix in the forwarding table
        // or the updates is that there isn't necessarily an Account that
        // corresponds to this ILP address)
        if start == 0 {
            new_routes.push(Route {
                prefix: self.ilp_address.read().to_bytes(),
                path: Vec::new(),
                // TODO what should we include here?
                auth: [0; 32],
                props: Vec::new(),
            });
        }

        // Iterate through each of the given epochs
        for (new, withdrawn) in forwarding_table_updates
            .iter()
            .skip(start)
            .take(epochs_to_take)
        {
            for new_route in new {
                new_routes.push(new_route.clone());
                // If the route was previously withdrawn, ignore that now since it was added back
                if withdrawn_routes.contains(&new_route.prefix) {
                    withdrawn_routes = withdrawn_routes
                        .into_iter()
                        .filter(|prefix| prefix != &new_route.prefix)
                        .collect();
                }
            }

            for withdrawn_route in withdrawn {
                withdrawn_routes.push(withdrawn_route.clone());
                // If the route was previously added, ignore that since it was withdrawn later
                if new_routes
                    .iter()
                    .any(|route| route.prefix == withdrawn_route)
                {
                    new_routes = new_routes
                        .into_iter()
                        .filter(|route| route.prefix != withdrawn_route)
                        .collect();
                }
            }
        }

        RouteUpdateRequest {
            routing_table_id,
            from_epoch_index,
            to_epoch_index,
            current_epoch_index,
            new_routes: new_routes.clone(),
            withdrawn_routes: withdrawn_routes.clone(),
            speaker: self.ilp_address.read().clone(),
            hold_down_time: DEFAULT_ROUTE_EXPIRY_TIME,
        }
    }

    /// Send a Route Update Request to a specific account for the given epoch range.
    /// This is used when the peer has fallen behind and has requested a specific range of updates.
    fn send_route_update(
        &self,
        account: A,
        from_epoch_index: u32,
        to_epoch_index: u32,
    ) -> impl Future<Item = (), Error = ()> {
        let prepare = self
            .create_route_update(from_epoch_index, to_epoch_index)
            .to_prepare();
        let account_id = account.id();
        debug!(
            "Sending individual route update to account: {} for epochs from: {} to: {}",
            account_id, from_epoch_index, to_epoch_index
        );
        self.outgoing
            .clone()
            .send_request(OutgoingRequest {
                from: account.clone(),
                to: account,
                original_amount: prepare.amount(),
                prepare,
            })
            .and_then(|_| Ok(()))
            .then(move |result| {
                if let Err(err) = result {
                    error!(
                        "Error sending route update to account {}: {:?}",
                        account_id, err
                    )
                }
                Ok(())
            })
    }
}

fn get_best_route_for_prefix<A: CcpRoutingAccount>(
    local_routes: &HashMap<Bytes, A>,
    configured_routes: &HashMap<Bytes, A>,
    incoming_tables: &HashMap<A::AccountId, RoutingTable<A>>,
    prefix: &[u8],
) -> Option<(A, Route)> {
    // Check if we have a configured route for that specific prefix
    // or any shorter prefix ("example.a.b.c" will match "example.a.b" and "example.a")
    // Note that this logic is duplicated from the Address type. We are not using
    // Addresses here because the prefixes may not be valid ILP addresses ("example." is
    // a valid prefix but not a valid address)
    let segments: Vec<&[u8]> = prefix.split(|c| c == &b'.').collect();
    for i in 0..segments.len() {
        let prefix = &segments[0..segments.len() - i].join(&b'.');
        if let Some(account) = configured_routes.get(prefix.as_ref() as &[u8]) {
            return Some((
                account.clone(),
                Route {
                    prefix: account.ilp_address().to_bytes(),
                    auth: [0; 32],
                    path: Vec::new(),
                    props: Vec::new(),
                },
            ));
        }
    }

    if let Some(account) = local_routes.get(prefix) {
        return Some((
            account.clone(),
            Route {
                prefix: account.ilp_address().to_bytes(),
                auth: [0; 32],
                path: Vec::new(),
                props: Vec::new(),
            },
        ));
    }

    let mut candidate_routes = incoming_tables
        .values()
        .filter_map(|incoming_table| incoming_table.get_route(prefix));
    if let Some((account, route)) = candidate_routes.next() {
        let (best_account, best_route) = candidate_routes.fold(
            (account, route),
            |(best_account, best_route), (account, route)| {
                // Prioritize child > peer > parent
                if best_account.routing_relation() > account.routing_relation() {
                    return (best_account, best_route);
                } else if best_account.routing_relation() < account.routing_relation() {
                    return (account, route);
                }

                // Prioritize shortest path
                if best_route.path.len() < route.path.len() {
                    return (best_account, best_route);
                } else if best_route.path.len() > route.path.len() {
                    return (account, route);
                }

                // Finally base it on account ID
                if best_account.id().to_string() < account.id().to_string() {
                    (best_account, best_route)
                } else {
                    (account, route)
                }
            },
        );
        Some((best_account.clone(), best_route.clone()))
    } else {
        None
    }
}

impl<I, O, S, A> IncomingService<A> for CcpRouteManager<I, O, S, A>
where
    I: IncomingService<A> + Clone + Send + Sync + 'static,
    O: OutgoingService<A> + Clone + Send + Sync + 'static,
    S: AddressStore + RouteManagerStore<Account = A> + Clone + Send + Sync + 'static,
    A: CcpRoutingAccount + Send + Sync + 'static,
{
    type Future = BoxedIlpFuture;

    /// Handle the IncomingRequest if it is a CCP protocol message or
    /// pass it on to the next handler if not
    fn handle_request(&mut self, request: IncomingRequest<A>) -> Self::Future {
        let destination = request.prepare.destination();
        if destination == *CCP_CONTROL_DESTINATION {
            Box::new(self.handle_route_control_request(request))
        } else if destination == *CCP_UPDATE_DESTINATION {
            Box::new(self.handle_route_update_request(request))
        } else {
            Box::new(self.next_incoming.handle_request(request))
        }
    }
}

#[cfg(test)]
mod ranking_routes {
    use super::*;
    use crate::test_helpers::*;
    use crate::RoutingRelation;
    use std::iter::FromIterator;

    lazy_static! {
        static ref LOCAL: HashMap<Bytes, TestAccount> = HashMap::from_iter(vec![
            (
                Bytes::from("example.a"),
                TestAccount::new(1, "example.local.one")
            ),
            (
                Bytes::from("example.b"),
                TestAccount::new(2, "example.local.two")
            ),
            (
                Bytes::from("example.c"),
                TestAccount::new(3, "example.local.three")
            ),
        ]);
        static ref CONFIGURED: HashMap<Bytes, TestAccount> = HashMap::from_iter(vec![
            (
                Bytes::from("example.a"),
                TestAccount::new(4, "example.local.four")
            ),
            (
                Bytes::from("example.b"),
                TestAccount::new(5, "example.local.five")
            ),
        ]);
        static ref INCOMING: HashMap<u64, RoutingTable<TestAccount>> = {
            let mut child_table = RoutingTable::default();
            let mut child = TestAccount::new(6, "example.child");
            child.relation = RoutingRelation::Child;
            child_table.add_route(
                child.clone(),
                Route {
                    prefix: Bytes::from("example.d"),
                    path: vec![Bytes::from("example.one")],
                    auth: [0; 32],
                    props: Vec::new(),
                },
            );
            let mut peer_table_1 = RoutingTable::default();
            let peer_1 = TestAccount::new(7, "example.peer1");
            peer_table_1.add_route(
                peer_1.clone(),
                Route {
                    prefix: Bytes::from("example.d"),
                    path: Vec::new(),
                    auth: [0; 32],
                    props: Vec::new(),
                },
            );
            peer_table_1.add_route(
                peer_1.clone(),
                Route {
                    prefix: Bytes::from("example.e"),
                    path: vec![Bytes::from("example.one")],
                    auth: [0; 32],
                    props: Vec::new(),
                },
            );
            peer_table_1.add_route(
                peer_1.clone(),
                Route {
                    // This route should be overridden by the configured "example.a" route
                    prefix: Bytes::from("example.a.sub-prefix"),
                    path: vec![Bytes::from("example.one")],
                    auth: [0; 32],
                    props: Vec::new(),
                },
            );
            let mut peer_table_2 = RoutingTable::default();
            let peer_2 = TestAccount::new(8, "example.peer2");
            peer_table_2.add_route(
                peer_2.clone(),
                Route {
                    prefix: Bytes::from("example.e"),
                    path: vec![Bytes::from("example.one"), Bytes::from("example.two")],
                    auth: [0; 32],
                    props: Vec::new(),
                },
            );
            HashMap::from_iter(vec![(6, child_table), (7, peer_table_1), (8, peer_table_2)])
        };
    }

    #[test]
    fn prioritizes_configured_routes() {
        let best_route = get_best_route_for_prefix(&LOCAL, &CONFIGURED, &INCOMING, b"example.a");
        assert_eq!(best_route.unwrap().0.id(), 4);
    }

    #[test]
    fn prioritizes_shorter_configured_routes() {
        let best_route =
            get_best_route_for_prefix(&LOCAL, &CONFIGURED, &INCOMING, b"example.a.sub-prefix");
        assert_eq!(best_route.unwrap().0.id(), 4);
    }

    #[test]
    fn prioritizes_local_routes_over_broadcasted_ones() {
        let best_route = get_best_route_for_prefix(&LOCAL, &CONFIGURED, &INCOMING, b"example.c");
        assert_eq!(best_route.unwrap().0.id(), 3);
    }

    #[test]
    fn prioritizes_children_over_peers() {
        let best_route = get_best_route_for_prefix(&LOCAL, &CONFIGURED, &INCOMING, b"example.d");
        assert_eq!(best_route.unwrap().0.id(), 6);
    }

    #[test]
    fn prioritizes_shorter_paths() {
        let best_route = get_best_route_for_prefix(&LOCAL, &CONFIGURED, &INCOMING, b"example.e");
        assert_eq!(best_route.unwrap().0.id(), 7);
    }

    #[test]
    fn returns_none_for_no_route() {
        let best_route = get_best_route_for_prefix(&LOCAL, &CONFIGURED, &INCOMING, b"example.z");
        assert!(best_route.is_none());
    }
}

#[cfg(test)]
mod handle_route_control_request {
    use super::*;
    use crate::fixtures::*;
    use crate::test_helpers::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn handles_valid_request() {
        test_service_with_routes()
            .0
            .handle_request(IncomingRequest {
                prepare: CONTROL_REQUEST.to_prepare(),
                from: ROUTING_ACCOUNT.clone(),
            })
            .wait()
            .unwrap();
    }

    #[test]
    fn rejects_from_non_sending_account() {
        let result = test_service()
            .handle_request(IncomingRequest {
                prepare: CONTROL_REQUEST.to_prepare(),
                from: NON_ROUTING_ACCOUNT.clone(),
            })
            .wait();
        assert!(result.is_err());
        assert_eq!(
            str::from_utf8(result.unwrap_err().message()).unwrap(),
            "We are not configured to send routes to you, sorry"
        );
    }

    #[test]
    fn rejects_invalid_packet() {
        let result = test_service()
            .handle_request(IncomingRequest {
                prepare: PrepareBuilder {
                    destination: CCP_CONTROL_DESTINATION.clone(),
                    amount: 0,
                    expires_at: SystemTime::now() + Duration::from_secs(30),
                    data: &[],
                    execution_condition: &PEER_PROTOCOL_CONDITION,
                }
                .build(),
                from: ROUTING_ACCOUNT.clone(),
            })
            .wait();
        assert!(result.is_err());
        assert_eq!(
            str::from_utf8(result.unwrap_err().message()).unwrap(),
            "Invalid route control request"
        );
    }

    #[test]
    fn sends_update_in_response() {
        let (mut service, outgoing_requests) = test_service_with_routes();
        (*service.forwarding_table.write()).set_id([0; 16]);
        service.update_best_routes(None).wait().unwrap();
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: RouteControlRequest {
                    last_known_routing_table_id: [0; 16],
                    mode: Mode::Sync,
                    last_known_epoch: 0,
                    features: Vec::new(),
                }
                .to_prepare(),
            })
            .wait()
            .unwrap();
        let request: &OutgoingRequest<TestAccount> = &outgoing_requests.lock()[0];
        assert_eq!(request.to.id(), ROUTING_ACCOUNT.id());
        let update = RouteUpdateRequest::try_from(&request.prepare).unwrap();
        assert_eq!(update.routing_table_id, [0; 16]);
        assert_eq!(update.from_epoch_index, 0);
        assert_eq!(update.to_epoch_index, 1);
        assert_eq!(update.current_epoch_index, 1);
        assert_eq!(update.new_routes.len(), 3);
    }

    #[test]
    fn sends_whole_table_if_id_is_different() {
        let (mut service, outgoing_requests) = test_service_with_routes();
        service.update_best_routes(None).wait().unwrap();
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: RouteControlRequest {
                    last_known_routing_table_id: [0; 16],
                    mode: Mode::Sync,
                    last_known_epoch: 32,
                    features: Vec::new(),
                }
                .to_prepare(),
            })
            .wait()
            .unwrap();
        let routing_table_id = service.forwarding_table.read().id();
        let request: &OutgoingRequest<TestAccount> = &outgoing_requests.lock()[0];
        assert_eq!(request.to.id(), ROUTING_ACCOUNT.id());
        let update = RouteUpdateRequest::try_from(&request.prepare).unwrap();
        assert_eq!(update.routing_table_id, routing_table_id);
        assert_eq!(update.from_epoch_index, 0);
        assert_eq!(update.to_epoch_index, 1);
        assert_eq!(update.current_epoch_index, 1);
        assert_eq!(update.new_routes.len(), 3);
    }
}

#[cfg(test)]
mod handle_route_update_request {
    use super::*;
    use crate::fixtures::*;
    use crate::test_helpers::*;
    use std::{
        iter::FromIterator,
        time::{Duration, SystemTime},
    };

    #[test]
    fn handles_valid_request() {
        let mut service = test_service();
        let mut update = UPDATE_REQUEST_SIMPLE.clone();
        update.to_epoch_index = 1;
        update.from_epoch_index = 0;

        service
            .handle_request(IncomingRequest {
                prepare: update.to_prepare(),
                from: ROUTING_ACCOUNT.clone(),
            })
            .wait()
            .unwrap();
    }

    #[test]
    fn rejects_from_child_account() {
        let result = test_service()
            .handle_request(IncomingRequest {
                prepare: UPDATE_REQUEST_SIMPLE.to_prepare(),
                from: CHILD_ACCOUNT.clone(),
            })
            .wait();
        assert!(result.is_err());
        assert_eq!(
            str::from_utf8(result.unwrap_err().message()).unwrap(),
            "Your route broadcasts are not accepted here",
        );
    }

    #[test]
    fn rejects_from_non_routing_account() {
        let result = test_service()
            .handle_request(IncomingRequest {
                prepare: UPDATE_REQUEST_SIMPLE.to_prepare(),
                from: NON_ROUTING_ACCOUNT.clone(),
            })
            .wait();
        assert!(result.is_err());
        assert_eq!(
            str::from_utf8(result.unwrap_err().message()).unwrap(),
            "Your route broadcasts are not accepted here",
        );
    }

    #[test]
    fn rejects_invalid_packet() {
        let result = test_service()
            .handle_request(IncomingRequest {
                prepare: PrepareBuilder {
                    destination: CCP_UPDATE_DESTINATION.clone(),
                    amount: 0,
                    expires_at: SystemTime::now() + Duration::from_secs(30),
                    data: &[],
                    execution_condition: &PEER_PROTOCOL_CONDITION,
                }
                .build(),
                from: ROUTING_ACCOUNT.clone(),
            })
            .wait();
        assert!(result.is_err());
        assert_eq!(
            str::from_utf8(result.unwrap_err().message()).unwrap(),
            "Invalid route update request"
        );
    }

    #[test]
    fn adds_table_on_first_request() {
        let mut service = test_service();
        let mut update = UPDATE_REQUEST_SIMPLE.clone();
        update.to_epoch_index = 1;
        update.from_epoch_index = 0;

        service
            .handle_request(IncomingRequest {
                prepare: update.to_prepare(),
                from: ROUTING_ACCOUNT.clone(),
            })
            .wait()
            .unwrap();
        assert_eq!(service.incoming_tables.read().len(), 1);
    }

    #[test]
    fn filters_routes_with_other_address_scheme() {
        let service = test_service();
        let mut request = UPDATE_REQUEST_SIMPLE.clone();
        request.new_routes.push(Route {
            prefix: Bytes::from("example.valid"),
            path: Vec::new(),
            auth: [0; 32],
            props: Vec::new(),
        });
        request.new_routes.push(Route {
            prefix: Bytes::from("other.prefix"),
            path: Vec::new(),
            auth: [0; 32],
            props: Vec::new(),
        });
        let request = service.filter_routes(request);
        assert_eq!(request.new_routes.len(), 1);
        assert_eq!(request.new_routes[0].prefix, Bytes::from("example.valid"));
    }

    #[test]
    fn filters_routes_for_address_scheme() {
        let service = test_service();
        let mut request = UPDATE_REQUEST_SIMPLE.clone();
        request.new_routes.push(Route {
            prefix: Bytes::from("example.valid"),
            path: Vec::new(),
            auth: [0; 32],
            props: Vec::new(),
        });
        request.new_routes.push(Route {
            prefix: Bytes::from("example."),
            path: Vec::new(),
            auth: [0; 32],
            props: Vec::new(),
        });
        let request = service.filter_routes(request);
        dbg!(request.new_routes.clone());
        assert_eq!(request.new_routes.len(), 1);
        assert_eq!(request.new_routes[0].prefix, Bytes::from("example.valid"));
    }

    #[test]
    fn filters_routing_loops() {
        let service = test_service();
        let mut request = UPDATE_REQUEST_SIMPLE.clone();
        request.new_routes.push(Route {
            prefix: Bytes::from("example.valid"),
            path: vec![
                Bytes::from("example.a"),
                service.ilp_address.read().to_bytes(),
                Bytes::from("example.b"),
            ],
            auth: [0; 32],
            props: Vec::new(),
        });
        request.new_routes.push(Route {
            prefix: Bytes::from("example.valid"),
            path: Vec::new(),
            auth: [0; 32],
            props: Vec::new(),
        });
        let request = service.filter_routes(request);
        assert_eq!(request.new_routes.len(), 1);
        assert_eq!(request.new_routes[0].prefix, Bytes::from("example.valid"));
    }

    #[test]
    fn filters_own_prefix_routes() {
        let service = test_service();
        let mut request = UPDATE_REQUEST_SIMPLE.clone();
        request.new_routes.push(Route {
            prefix: Bytes::from("example.connector.invalid-route"),
            path: Vec::new(),
            auth: [0; 32],
            props: Vec::new(),
        });
        request.new_routes.push(Route {
            prefix: Bytes::from("example.valid"),
            path: Vec::new(),
            auth: [0; 32],
            props: Vec::new(),
        });
        let request = service.filter_routes(request);
        assert_eq!(request.new_routes.len(), 1);
        assert_eq!(request.new_routes[0].prefix, Bytes::from("example.valid"));
    }

    #[test]
    fn updates_local_routing_table() {
        let mut service = test_service();
        let mut request = UPDATE_REQUEST_COMPLEX.clone();
        request.to_epoch_index = 1;
        request.from_epoch_index = 0;
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: request.to_prepare(),
            })
            .wait()
            .unwrap();
        assert_eq!(
            (*service.local_table.read())
                .get_route(b"example.prefix1")
                .unwrap()
                .0
                .id(),
            ROUTING_ACCOUNT.id()
        );
        assert_eq!(
            (*service.local_table.read())
                .get_route(b"example.prefix2")
                .unwrap()
                .0
                .id(),
            ROUTING_ACCOUNT.id()
        );
    }

    #[test]
    fn writes_local_routing_table_to_store() {
        let mut service = test_service();
        let mut request = UPDATE_REQUEST_COMPLEX.clone();
        request.to_epoch_index = 1;
        request.from_epoch_index = 0;
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: request.to_prepare(),
            })
            .wait()
            .unwrap();
        assert_eq!(
            service
                .store
                .routes
                .lock()
                .get(&b"example.prefix1"[..])
                .unwrap()
                .id(),
            ROUTING_ACCOUNT.id()
        );
        assert_eq!(
            service
                .store
                .routes
                .lock()
                .get(&b"example.prefix2"[..])
                .unwrap()
                .id(),
            ROUTING_ACCOUNT.id()
        );
    }

    #[test]
    fn doesnt_overwrite_configured_or_local_routes() {
        let mut service = test_service();
        let store = TestStore::with_routes(
            HashMap::from_iter(vec![(
                Bytes::from("example.prefix1"),
                TestAccount::new(9, "example.account9"),
            )]),
            HashMap::from_iter(vec![(
                Bytes::from("example.prefix2"),
                TestAccount::new(10, "example.account10"),
            )]),
        );
        service.store = store;

        let mut request = UPDATE_REQUEST_COMPLEX.clone();
        request.to_epoch_index = 1;
        request.from_epoch_index = 0;
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: request.to_prepare(),
            })
            .wait()
            .unwrap();
        assert_eq!(
            (*service.local_table.read())
                .get_route(b"example.prefix1")
                .unwrap()
                .0
                .id(),
            9
        );
        assert_eq!(
            (*service.local_table.read())
                .get_route(b"example.prefix2")
                .unwrap()
                .0
                .id(),
            10
        );
    }

    #[test]
    fn removes_withdrawn_routes() {
        let mut service = test_service();
        let mut request = UPDATE_REQUEST_COMPLEX.clone();
        request.to_epoch_index = 1;
        request.from_epoch_index = 0;
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: request.to_prepare(),
            })
            .wait()
            .unwrap();
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: RouteUpdateRequest {
                    routing_table_id: UPDATE_REQUEST_COMPLEX.routing_table_id,
                    from_epoch_index: 1,
                    to_epoch_index: 3,
                    current_epoch_index: 3,
                    hold_down_time: 45000,
                    speaker: UPDATE_REQUEST_COMPLEX.speaker.clone(),
                    new_routes: Vec::new(),
                    withdrawn_routes: vec![Bytes::from("example.prefix2")],
                }
                .to_prepare(),
            })
            .wait()
            .unwrap();

        assert_eq!(
            (*service.local_table.read())
                .get_route(b"example.prefix1")
                .unwrap()
                .0
                .id(),
            ROUTING_ACCOUNT.id()
        );
        assert!((*service.local_table.read())
            .get_route(b"example.prefix2")
            .is_none());
    }

    #[test]
    fn sends_control_request_if_routing_table_id_changed() {
        let (mut service, outgoing_requests) = test_service_with_routes();
        // First request is valid
        let mut request1 = UPDATE_REQUEST_COMPLEX.clone();
        request1.to_epoch_index = 3;
        request1.from_epoch_index = 0;
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: request1.to_prepare(),
            })
            .wait()
            .unwrap();

        // Second has a gap in epochs
        let mut request2 = UPDATE_REQUEST_COMPLEX.clone();
        request2.to_epoch_index = 8;
        request2.from_epoch_index = 7;
        request2.routing_table_id = [9; 16];
        let err = service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: request2.to_prepare(),
            })
            .wait()
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::F00_BAD_REQUEST);

        let request = &outgoing_requests.lock()[0];
        let control = RouteControlRequest::try_from(&request.prepare).unwrap();
        assert_eq!(control.last_known_epoch, 0);
        assert_eq!(
            control.last_known_routing_table_id,
            request2.routing_table_id
        );
    }

    #[test]
    fn sends_control_request_if_missing_epochs() {
        let (mut service, outgoing_requests) = test_service_with_routes();

        // First request is valid
        let mut request = UPDATE_REQUEST_COMPLEX.clone();
        request.to_epoch_index = 1;
        request.from_epoch_index = 0;
        service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: request.to_prepare(),
            })
            .wait()
            .unwrap();

        // Second has a gap in epochs
        let mut request = UPDATE_REQUEST_COMPLEX.clone();
        request.to_epoch_index = 8;
        request.from_epoch_index = 7;
        let err = service
            .handle_request(IncomingRequest {
                from: ROUTING_ACCOUNT.clone(),
                prepare: request.to_prepare(),
            })
            .wait()
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::F00_BAD_REQUEST);

        let request = &outgoing_requests.lock()[0];
        let control = RouteControlRequest::try_from(&request.prepare).unwrap();
        assert_eq!(control.last_known_epoch, 1);
    }
}

#[cfg(test)]
mod create_route_update {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn heartbeat_message_for_empty_table() {
        let service = test_service();
        let update = service.create_route_update(0, 0);
        assert_eq!(update.from_epoch_index, 0);
        assert_eq!(update.to_epoch_index, 0);
        assert_eq!(update.current_epoch_index, 0);
        // Connector's own route is always included in the 0 epoch
        assert_eq!(update.new_routes.len(), 1);
        assert_eq!(update.new_routes[0].prefix, "example.connector");
        assert!(update.withdrawn_routes.is_empty());
    }

    #[test]
    fn includes_the_given_range_of_epochs() {
        let service = test_service();
        (*service.forwarding_table.write()).set_epoch(4);
        *service.forwarding_table_updates.write() = vec![
            (
                vec![Route {
                    prefix: Bytes::from("example.a"),
                    path: vec![Bytes::from("example.x")],
                    auth: [1; 32],
                    props: Vec::new(),
                }],
                Vec::new(),
            ),
            (
                vec![Route {
                    prefix: Bytes::from("example.b"),
                    path: vec![Bytes::from("example.x")],
                    auth: [2; 32],
                    props: Vec::new(),
                }],
                Vec::new(),
            ),
            (
                vec![Route {
                    prefix: Bytes::from("example.c"),
                    path: vec![Bytes::from("example.x"), Bytes::from("example.y")],
                    auth: [3; 32],
                    props: Vec::new(),
                }],
                vec![Bytes::from("example.m")],
            ),
            (
                vec![Route {
                    prefix: Bytes::from("example.d"),
                    path: vec![Bytes::from("example.x"), Bytes::from("example.y")],
                    auth: [4; 32],
                    props: Vec::new(),
                }],
                vec![Bytes::from("example.n")],
            ),
        ];
        let update = service.create_route_update(1, 3);
        assert_eq!(update.from_epoch_index, 1);
        assert_eq!(update.to_epoch_index, 3);
        assert_eq!(update.current_epoch_index, 4);
        assert_eq!(update.new_routes.len(), 2);
        assert_eq!(update.withdrawn_routes.len(), 1);
        let new_routes: Vec<&str> = update
            .new_routes
            .iter()
            .map(|r| str::from_utf8(r.prefix.as_ref()).unwrap())
            .collect();
        assert!(new_routes.contains(&"example.b"));
        assert!(new_routes.contains(&"example.c"));
        assert!(!new_routes.contains(&"example.m"));
        assert_eq!(update.withdrawn_routes[0], &Bytes::from("example.m"));
    }
}

#[cfg(test)]
mod send_route_updates {
    use super::*;
    use crate::fixtures::*;
    use crate::test_helpers::*;
    use interledger_service::*;
    use std::{iter::FromIterator, str::FromStr};

    #[test]
    fn broadcasts_to_all_accounts_we_send_updates_to() {
        let (service, outgoing_requests) = test_service_with_routes();
        service.send_route_updates().wait().unwrap();
        let mut accounts: Vec<u64> = outgoing_requests
            .lock()
            .iter()
            .map(|request| request.to.id())
            .collect();
        accounts.sort_unstable();
        assert_eq!(accounts, vec![1, 2]);
    }

    #[test]
    fn broadcasts_configured_and_local_routes() {
        let (service, outgoing_requests) = test_service_with_routes();

        // This is normally spawned as a task when the service is created
        service.update_best_routes(None).wait().unwrap();

        service.send_route_updates().wait().unwrap();
        let update = RouteUpdateRequest::try_from(&outgoing_requests.lock()[0].prepare).unwrap();
        assert_eq!(update.new_routes.len(), 3);
        let prefixes: Vec<&str> = update
            .new_routes
            .iter()
            .map(|route| str::from_utf8(route.prefix.as_ref()).unwrap())
            .collect();
        assert!(prefixes.contains(&"example.local.1"));
        assert!(prefixes.contains(&"example.configured.1"));
    }

    #[test]
    fn broadcasts_received_routes() {
        let (service, outgoing_requests) = test_service_with_routes();

        // This is normally spawned as a task when the service is created
        service.update_best_routes(None).wait().unwrap();

        service
            .handle_route_update_request(IncomingRequest {
                from: TestAccount::new(10, "example.peer"),
                prepare: RouteUpdateRequest {
                    routing_table_id: [0; 16],
                    current_epoch_index: 1,
                    from_epoch_index: 0,
                    to_epoch_index: 1,
                    hold_down_time: 30000,
                    speaker: Address::from_str("example.remote").unwrap(),
                    new_routes: vec![Route {
                        prefix: Bytes::from("example.remote"),
                        path: vec![Bytes::from("example.peer")],
                        auth: [0; 32],
                        props: Vec::new(),
                    }],
                    withdrawn_routes: Vec::new(),
                }
                .to_prepare(),
            })
            .wait()
            .unwrap();

        service.send_route_updates().wait().unwrap();
        let update = RouteUpdateRequest::try_from(&outgoing_requests.lock()[0].prepare).unwrap();
        assert_eq!(update.new_routes.len(), 4);
        let prefixes: Vec<&str> = update
            .new_routes
            .iter()
            .map(|route| str::from_utf8(route.prefix.as_ref()).unwrap())
            .collect();
        assert!(prefixes.contains(&"example.local.1"));
        assert!(prefixes.contains(&"example.configured.1"));
        assert!(prefixes.contains(&"example.remote"));
    }

    #[test]
    fn broadcasts_withdrawn_routes() {
        let (service, outgoing_requests) = test_service_with_routes();

        // This is normally spawned as a task when the service is created
        service.update_best_routes(None).wait().unwrap();

        service
            .handle_route_update_request(IncomingRequest {
                from: TestAccount::new(10, "example.peer"),
                prepare: RouteUpdateRequest {
                    routing_table_id: [0; 16],
                    current_epoch_index: 1,
                    from_epoch_index: 0,
                    to_epoch_index: 1,
                    hold_down_time: 30000,
                    speaker: Address::from_str("example.remote").unwrap(),
                    new_routes: vec![Route {
                        prefix: Bytes::from("example.remote"),
                        path: vec![Bytes::from("example.peer")],
                        auth: [0; 32],
                        props: Vec::new(),
                    }],
                    withdrawn_routes: Vec::new(),
                }
                .to_prepare(),
            })
            .wait()
            .unwrap();
        service
            .handle_route_update_request(IncomingRequest {
                from: TestAccount::new(10, "example.peer"),
                prepare: RouteUpdateRequest {
                    routing_table_id: [0; 16],
                    current_epoch_index: 4,
                    from_epoch_index: 1,
                    to_epoch_index: 4,
                    hold_down_time: 30000,
                    speaker: Address::from_str("example.remote").unwrap(),
                    new_routes: Vec::new(),
                    withdrawn_routes: vec![Bytes::from("example.remote")],
                }
                .to_prepare(),
            })
            .wait()
            .unwrap();

        service.send_route_updates().wait().unwrap();
        let update = RouteUpdateRequest::try_from(&outgoing_requests.lock()[0].prepare).unwrap();
        assert_eq!(update.new_routes.len(), 3);
        let prefixes: Vec<&str> = update
            .new_routes
            .iter()
            .map(|route| str::from_utf8(route.prefix.as_ref()).unwrap())
            .collect();
        assert!(prefixes.contains(&"example.local.1"));
        assert!(prefixes.contains(&"example.configured.1"));
        assert!(!prefixes.contains(&"example.remote"));
        assert_eq!(update.withdrawn_routes.len(), 1);
        assert_eq!(
            str::from_utf8(&update.withdrawn_routes[0]).unwrap(),
            "example.remote"
        );
    }

    #[test]
    fn backs_off_sending_to_unavailable_child_accounts() {
        let local_routes = HashMap::from_iter(vec![
            (
                Bytes::from("example.local.1"),
                TestAccount::new(1, "example.local.1"),
            ),
            (
                Bytes::from("example.connector.other-local"),
                TestAccount {
                    id: 2,
                    ilp_address: Address::from_str("example.connector.other-local").unwrap(),
                    relation: RoutingRelation::Child,
                },
            ),
        ]);
        let store = TestStore::with_routes(local_routes, HashMap::new());
        let outgoing_requests: Arc<Mutex<Vec<OutgoingRequest<TestAccount>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let outgoing_requests_clone = outgoing_requests.clone();
        let outgoing = outgoing_service_fn(move |request: OutgoingRequest<TestAccount>| {
            let res = if request.to.routing_relation() == RoutingRelation::Child {
                Err(RejectBuilder {
                    code: ErrorCode::F00_BAD_REQUEST,
                    message: &[],
                    data: &[],
                    triggered_by: Some(request.to.ilp_address()),
                }
                .build())
            } else {
                Ok(CCP_RESPONSE.clone())
            };
            (*outgoing_requests_clone.lock()).push(request);
            res
        });
        let service = CcpRouteManagerBuilder::new(
            Address::from_str("example.connector").unwrap(),
            store,
            outgoing,
            incoming_service_fn(|_request| {
                Box::new(err(RejectBuilder {
                    code: ErrorCode::F02_UNREACHABLE,
                    message: b"No other incoming handler!",
                    data: &[],
                    triggered_by: Some(&EXAMPLE_CONNECTOR),
                }
                .build()))
            }),
        )
        .ilp_address(Address::from_str("example.connector").unwrap())
        .to_service();
        service.send_route_updates().wait().unwrap();

        // The first time, the child request is rejected
        assert_eq!(outgoing_requests.lock().len(), 2);
        {
            let lock = service.unavailable_accounts.lock();
            let backoff = lock
                .get(&2)
                .expect("Should have added chlid to unavailable accounts");
            assert_eq!(backoff.max, 1);
            assert_eq!(backoff.skip_intervals, 1);
        }

        *outgoing_requests.lock() = Vec::new();
        service.send_route_updates().wait().unwrap();

        // When we send again, we skip the child
        assert_eq!(outgoing_requests.lock().len(), 1);
        {
            let lock = service.unavailable_accounts.lock();
            let backoff = lock
                .get(&2)
                .expect("Should have added chlid to unavailable accounts");
            assert_eq!(backoff.max, 1);
            assert_eq!(backoff.skip_intervals, 0);
        }

        *outgoing_requests.lock() = Vec::new();
        service.send_route_updates().wait().unwrap();

        // When we send again, we try the child but it still won't work
        assert_eq!(outgoing_requests.lock().len(), 2);
        {
            let lock = service.unavailable_accounts.lock();
            let backoff = lock
                .get(&2)
                .expect("Should have added chlid to unavailable accounts");
            assert_eq!(backoff.max, 2);
            assert_eq!(backoff.skip_intervals, 2);
        }
    }

    #[test]
    fn resets_backoff_on_route_control_request() {
        let child_account = TestAccount {
            id: 2,
            ilp_address: Address::from_str("example.connector.other-local").unwrap(),
            relation: RoutingRelation::Child,
        };
        let local_routes = HashMap::from_iter(vec![
            (
                Bytes::from("example.local.1"),
                TestAccount::new(1, "example.local.1"),
            ),
            (
                Bytes::from("example.connector.other-local"),
                child_account.clone(),
            ),
        ]);
        let store = TestStore::with_routes(local_routes, HashMap::new());
        let outgoing_requests: Arc<Mutex<Vec<OutgoingRequest<TestAccount>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let outgoing_requests_clone = outgoing_requests.clone();
        let outgoing = outgoing_service_fn(move |request: OutgoingRequest<TestAccount>| {
            let res = if request.to.routing_relation() == RoutingRelation::Child {
                Err(RejectBuilder {
                    code: ErrorCode::F00_BAD_REQUEST,
                    message: &[],
                    data: &[],
                    triggered_by: Some(request.to.ilp_address()),
                }
                .build())
            } else {
                Ok(CCP_RESPONSE.clone())
            };
            (*outgoing_requests_clone.lock()).push(request);
            res
        });
        let mut service = CcpRouteManagerBuilder::new(
            Address::from_str("example.connector").unwrap(),
            store,
            outgoing,
            incoming_service_fn(|_request| {
                Box::new(err(RejectBuilder {
                    code: ErrorCode::F02_UNREACHABLE,
                    message: b"No other incoming handler!",
                    data: &[],
                    triggered_by: Some(&EXAMPLE_CONNECTOR),
                }
                .build()))
            }),
        )
        .ilp_address(Address::from_str("example.connector").unwrap())
        .to_service();
        service.send_route_updates().wait().unwrap();

        // The first time, the child request is rejected
        assert_eq!(outgoing_requests.lock().len(), 2);
        {
            let lock = service.unavailable_accounts.lock();
            let backoff = lock
                .get(&2)
                .expect("Should have added chlid to unavailable accounts");
            assert_eq!(backoff.max, 1);
            assert_eq!(backoff.skip_intervals, 1);
        }

        service
            .handle_request(IncomingRequest {
                prepare: CONTROL_REQUEST.to_prepare(),
                from: child_account,
            })
            .wait()
            .unwrap();
        {
            let lock = service.unavailable_accounts.lock();
            assert!(lock.get(&2).is_none());
        }

        *outgoing_requests.lock() = Vec::new();
        service.send_route_updates().wait().unwrap();

        // When we send again, we don't skip the child because we got a request from them
        assert_eq!(outgoing_requests.lock().len(), 2);
    }
}
