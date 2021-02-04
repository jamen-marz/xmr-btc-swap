//! Run an XMR/BTC swap in the role of Alice.
//! Alice holds XMR and wishes receive BTC.
use crate::{
    bitcoin, database,
    database::Database,
    execution_params::ExecutionParams,
    monero,
    network::{
        peer_tracker::{self, PeerTracker},
        transport::build,
        Seed as NetworkSeed,
    },
    protocol::{bob::EncryptedSignature, SwapAmounts},
    seed::Seed,
};
use anyhow::{bail, Result};
use libp2p::{
    core::Multiaddr, identity::Keypair, request_response::ResponseChannel, NetworkBehaviour, PeerId,
};
use rand::rngs::OsRng;
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, info};
use uuid::Uuid;

pub use self::{
    event_loop::{EventLoop, EventLoopHandle},
    execution_setup::Message1,
    state::*,
    swap::{run, run_until},
    swap_response::*,
    transfer_proof::TransferProof,
};
use crate::network::request_response::Response;
pub use execution_setup::Message3;

mod encrypted_signature;
pub mod event_loop;
mod execution_setup;
pub mod state;
mod steps;
pub mod swap;
mod swap_response;
mod transfer_proof;

pub struct Swap {
    pub state: AliceState,
    pub event_loop_handle: EventLoopHandle,
    pub bitcoin_wallet: Arc<bitcoin::Wallet>,
    pub monero_wallet: Arc<monero::Wallet>,
    pub execution_params: ExecutionParams,
    pub swap_id: Uuid,
    pub db: Database,
}

pub struct Builder {
    swap_id: Uuid,
    identity: Keypair,
    peer_id: PeerId,
    db_path: PathBuf,
    execution_params: ExecutionParams,

    listen_address: Multiaddr,

    bitcoin_wallet: Arc<bitcoin::Wallet>,
    monero_wallet: Arc<monero::Wallet>,

    init_params: InitParams,
}

enum InitParams {
    None,
    New { swap_amounts: SwapAmounts },
}

impl Builder {
    pub fn new(
        seed: Seed,
        execution_params: ExecutionParams,
        swap_id: Uuid,
        bitcoin_wallet: Arc<bitcoin::Wallet>,
        monero_wallet: Arc<monero::Wallet>,
        db_path: PathBuf,
        listen_address: Multiaddr,
    ) -> Self {
        let network_seed = NetworkSeed::new(seed);
        let identity = network_seed.derive_libp2p_identity();
        let peer_id = PeerId::from(identity.public());

        Self {
            swap_id,
            identity,
            peer_id,
            db_path,
            execution_params,
            listen_address,
            bitcoin_wallet,
            monero_wallet,
            init_params: InitParams::None,
        }
    }

    pub fn with_init_params(self, swap_amounts: SwapAmounts) -> Self {
        Self {
            init_params: InitParams::New { swap_amounts },
            ..self
        }
    }

    pub async fn build(self) -> Result<(Swap, EventLoop)> {
        match self.init_params {
            InitParams::New { swap_amounts } => {
                let initial_state = self
                    .make_initial_state(swap_amounts.btc, swap_amounts.xmr)
                    .await?;

                let (event_loop, event_loop_handle) = self.init_event_loop()?;

                let db = Database::open(self.db_path.as_path())?;

                Ok((
                    Swap {
                        event_loop_handle,
                        bitcoin_wallet: self.bitcoin_wallet,
                        monero_wallet: self.monero_wallet,
                        execution_params: self.execution_params,
                        db,
                        state: initial_state,
                        swap_id: self.swap_id,
                    },
                    event_loop,
                ))
            }
            InitParams::None => {
                // reopen the existing database
                let db = Database::open(self.db_path.as_path())?;

                let resume_state =
                    if let database::Swap::Alice(state) = db.get_state(self.swap_id)? {
                        state.into()
                    } else {
                        bail!(
                            "Trying to load swap with id {} for the wrong direction.",
                            self.swap_id
                        )
                    };

                let (event_loop, event_loop_handle) = self.init_event_loop()?;

                Ok((
                    Swap {
                        state: resume_state,
                        event_loop_handle,
                        bitcoin_wallet: self.bitcoin_wallet,
                        monero_wallet: self.monero_wallet,
                        execution_params: self.execution_params,
                        swap_id: self.swap_id,
                        db,
                    },
                    event_loop,
                ))
            }
        }
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    pub fn listen_address(&self) -> Multiaddr {
        self.listen_address.clone()
    }

    async fn make_initial_state(
        &self,
        btc_to_swap: bitcoin::Amount,
        xmr_to_swap: monero::Amount,
    ) -> Result<AliceState> {
        let rng = &mut OsRng;

        let amounts = SwapAmounts {
            btc: btc_to_swap,
            xmr: xmr_to_swap,
        };

        let a = bitcoin::SecretKey::new_random(rng);
        let s_a = cross_curve_dleq::Scalar::random(rng);
        let v_a = monero::PrivateViewKey::new_random(rng);
        let redeem_address = self.bitcoin_wallet.new_address().await?;
        let punish_address = redeem_address.clone();
        let state0 = State0::new(
            a,
            s_a,
            v_a,
            amounts.btc,
            amounts.xmr,
            self.execution_params.bitcoin_cancel_timelock,
            self.execution_params.bitcoin_punish_timelock,
            redeem_address,
            punish_address,
            rng,
        );

        Ok(AliceState::Started { amounts, state0 })
    }

    fn init_event_loop(&self) -> Result<(EventLoop, EventLoopHandle)> {
        let alice_behaviour = Behaviour::default();
        let alice_transport = build(self.identity.clone())?;
        EventLoop::new(
            alice_transport,
            alice_behaviour,
            self.listen_address(),
            self.peer_id,
        )
    }
}

#[derive(Debug)]
pub enum OutEvent {
    ConnectionEstablished(PeerId),
    Request(Box<swap_response::OutEvent>),
    ExecutionSetupDone(Result<Box<State3>>),
    TransferProofAcknowledged,
    EncryptedSignature(Box<EncryptedSignature>),
}

impl From<peer_tracker::OutEvent> for OutEvent {
    fn from(event: peer_tracker::OutEvent) -> Self {
        match event {
            peer_tracker::OutEvent::ConnectionEstablished(id) => {
                OutEvent::ConnectionEstablished(id)
            }
        }
    }
}

impl From<swap_response::OutEvent> for OutEvent {
    fn from(event: swap_response::OutEvent) -> Self {
        OutEvent::Request(Box::new(event))
    }
}

impl From<execution_setup::OutEvent> for OutEvent {
    fn from(event: execution_setup::OutEvent) -> Self {
        match event {
            execution_setup::OutEvent::Done(res) => OutEvent::ExecutionSetupDone(res.map(Box::new)),
        }
    }
}

impl From<transfer_proof::OutEvent> for OutEvent {
    fn from(event: transfer_proof::OutEvent) -> Self {
        match event {
            transfer_proof::OutEvent::Acknowledged => OutEvent::TransferProofAcknowledged,
        }
    }
}

impl From<encrypted_signature::OutEvent> for OutEvent {
    fn from(event: encrypted_signature::OutEvent) -> Self {
        match event {
            encrypted_signature::OutEvent::Msg(msg) => OutEvent::EncryptedSignature(Box::new(msg)),
        }
    }
}

/// A `NetworkBehaviour` that represents an XMR/BTC swap node as Alice.
#[derive(NetworkBehaviour, Default)]
#[behaviour(out_event = "OutEvent", event_process = false)]
#[allow(missing_debug_implementations)]
pub struct Behaviour {
    pt: PeerTracker,
    amounts: swap_response::Behaviour,
    execution_setup: execution_setup::Behaviour,
    transfer_proof: transfer_proof::Behaviour,
    encrypted_signature: encrypted_signature::Behaviour,
}

impl Behaviour {
    /// Alice always sends her messages as a response to a request from Bob.
    pub fn send_swap_response(
        &mut self,
        channel: ResponseChannel<Response>,
        swap_response: SwapResponse,
    ) -> Result<()> {
        self.amounts.send(channel, swap_response)?;
        info!("Sent swap response");
        Ok(())
    }

    pub fn start_execution_setup(&mut self, bob_peer_id: PeerId, state0: State0) {
        self.execution_setup.run(bob_peer_id, state0);
        info!("Start execution setup with {}", bob_peer_id);
    }

    /// Send Transfer Proof to Bob.
    pub fn send_transfer_proof(&mut self, bob: PeerId, msg: TransferProof) {
        self.transfer_proof.send(bob, msg);
        debug!("Sent Transfer Proof");
    }
}
