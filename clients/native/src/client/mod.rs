// Copyright 2021 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

use client_core::client::cover_traffic_stream::LoopCoverTrafficStream;
use client_core::client::inbound_messages::{
    InputMessage, InputMessageReceiver, InputMessageSender,
};
use client_core::client::key_manager::KeyManager;
use client_core::client::mix_traffic::{BatchMixMessageSender, MixTrafficController};
use client_core::client::real_messages_control;
use client_core::client::real_messages_control::RealMessagesController;
use client_core::client::received_buffer::{
    ReceivedBufferMessage, ReceivedBufferRequestReceiver, ReceivedBufferRequestSender,
    ReceivedMessagesBufferController, ReconstructedMessagesReceiver,
};
use client_core::client::reply_key_storage::ReplyKeyStorage;
use client_core::client::topology_control::{
    TopologyAccessor, TopologyRefresher, TopologyRefresherConfig,
};
use client_core::config::persistence::key_pathfinder::ClientKeyPathfinder;
use client_core::error::ClientCoreError;
use crypto::asymmetric::identity;
use futures::channel::mpsc;
use gateway_client::bandwidth::BandwidthController;
use gateway_client::{
    AcknowledgementReceiver, AcknowledgementSender, GatewayClient, MixnetMessageReceiver,
    MixnetMessageSender,
};
use log::*;
use nymsphinx::addressing::clients::Recipient;
use nymsphinx::addressing::nodes::NodeIdentity;
use nymsphinx::anonymous_replies::ReplySurb;
use nymsphinx::params::PacketSize;
use nymsphinx::receiver::ReconstructedMessage;
use task::{wait_for_signal, ShutdownListener, ShutdownNotifier};

use crate::client::config::{Config, SocketType};
use crate::error::ClientError;
use crate::websocket;

pub(crate) mod config;

pub struct NymClient {
    /// Client configuration options, including, among other things, packet sending rates,
    /// key filepaths, etc.
    config: Config,

    /// KeyManager object containing smart pointers to all relevant keys used by the client.
    key_manager: KeyManager,

    /// Channel used for transforming 'raw' messages into sphinx packets and sending them
    /// through the mix network.
    /// It is only available if the client started with the websocket listener disabled.
    input_tx: Option<InputMessageSender>,

    /// Channel used for obtaining reconstructed messages received from the mix network.
    /// It is only available if the client started with the websocket listener disabled.
    receive_tx: Option<ReconstructedMessagesReceiver>,
}

impl NymClient {
    pub fn new(config: Config) -> Self {
        let pathfinder = ClientKeyPathfinder::new_from_config(config.get_base());
        let key_manager = KeyManager::load_keys(&pathfinder).expect("failed to load stored keys");

        NymClient {
            config,
            key_manager,
            input_tx: None,
            receive_tx: None,
        }
    }

    pub fn as_mix_recipient(&self) -> Recipient {
        Recipient::new(
            *self.key_manager.identity_keypair().public_key(),
            *self.key_manager.encryption_keypair().public_key(),
            // TODO: below only works under assumption that gateway address == gateway id
            // (which currently is true)
            NodeIdentity::from_base58_string(self.config.get_base().get_gateway_id()).unwrap(),
        )
    }

    // future constantly pumping loop cover traffic at some specified average rate
    // the pumped traffic goes to the MixTrafficController
    fn start_cover_traffic_stream(
        &self,
        topology_accessor: TopologyAccessor,
        mix_tx: BatchMixMessageSender,
        shutdown: ShutdownListener,
    ) {
        info!("Starting loop cover traffic stream...");

        let mut stream = LoopCoverTrafficStream::new(
            self.key_manager.ack_key(),
            self.config.get_base().get_average_ack_delay(),
            self.config.get_base().get_average_packet_delay(),
            self.config
                .get_base()
                .get_loop_cover_traffic_average_delay(),
            mix_tx,
            self.as_mix_recipient(),
            topology_accessor,
        );

        if self.config.get_base().get_use_extended_packet_size() {
            stream.set_custom_packet_size(PacketSize::ExtendedPacket)
        }

        stream.start_with_shutdown(shutdown);
    }

    fn start_real_traffic_controller(
        &self,
        topology_accessor: TopologyAccessor,
        reply_key_storage: ReplyKeyStorage,
        ack_receiver: AcknowledgementReceiver,
        input_receiver: InputMessageReceiver,
        mix_sender: BatchMixMessageSender,
        shutdown: ShutdownListener,
    ) {
        let mut controller_config = real_messages_control::Config::new(
            self.key_manager.ack_key(),
            self.config.get_base().get_ack_wait_multiplier(),
            self.config.get_base().get_ack_wait_addition(),
            self.config.get_base().get_average_ack_delay(),
            self.config.get_base().get_message_sending_average_delay(),
            self.config.get_base().get_average_packet_delay(),
            self.config
                .get_base()
                .get_disabled_main_poisson_packet_distribution(),
            self.as_mix_recipient(),
        );

        if self.config.get_base().get_use_extended_packet_size() {
            controller_config.set_custom_packet_size(PacketSize::ExtendedPacket)
        }

        info!("Starting real traffic stream...");

        RealMessagesController::new(
            controller_config,
            ack_receiver,
            input_receiver,
            mix_sender,
            topology_accessor,
            reply_key_storage,
        )
        .start_with_shutdown(shutdown);
    }

    // buffer controlling all messages fetched from provider
    // required so that other components would be able to use them (say the websocket)
    fn start_received_messages_buffer_controller(
        &self,
        query_receiver: ReceivedBufferRequestReceiver,
        mixnet_receiver: MixnetMessageReceiver,
        reply_key_storage: ReplyKeyStorage,
        shutdown: ShutdownListener,
    ) {
        info!("Starting received messages buffer controller...");
        ReceivedMessagesBufferController::new(
            self.key_manager.encryption_keypair(),
            query_receiver,
            mixnet_receiver,
            reply_key_storage,
        )
        .start_with_shutdown(shutdown)
    }

    async fn start_gateway_client(
        &mut self,
        mixnet_message_sender: MixnetMessageSender,
        ack_sender: AcknowledgementSender,
        shutdown: ShutdownListener,
    ) -> GatewayClient {
        let gateway_id = self.config.get_base().get_gateway_id();
        if gateway_id.is_empty() {
            panic!("The identity of the gateway is unknown - did you run `nym-client` init?")
        }
        let gateway_owner = self.config.get_base().get_gateway_owner();
        if gateway_owner.is_empty() {
            panic!("The owner of the gateway is unknown - did you run `nym-client` init?")
        }
        let gateway_address = self.config.get_base().get_gateway_listener();
        if gateway_address.is_empty() {
            panic!("The address of the gateway is unknown - did you run `nym-client` init?")
        }

        let gateway_identity = identity::PublicKey::from_base58_string(gateway_id)
            .expect("provided gateway id is invalid!");

        #[cfg(feature = "coconut")]
        let bandwidth_controller = BandwidthController::new(
            credential_storage::initialise_storage(self.config.get_base().get_database_path())
                .await,
            self.config.get_base().get_validator_api_endpoints(),
        );
        #[cfg(not(feature = "coconut"))]
        let bandwidth_controller = BandwidthController::new(
            credential_storage::initialise_storage(self.config.get_base().get_database_path())
                .await,
        )
        .expect("Could not create bandwidth controller");

        let mut gateway_client = GatewayClient::new(
            gateway_address,
            self.key_manager.identity_keypair(),
            gateway_identity,
            gateway_owner,
            Some(self.key_manager.gateway_shared_key()),
            mixnet_message_sender,
            ack_sender,
            self.config.get_base().get_gateway_response_timeout(),
            Some(bandwidth_controller),
            Some(shutdown),
        );

        gateway_client
            .set_disabled_credentials_mode(self.config.get_base().get_disabled_credentials_mode());

        gateway_client
            .authenticate_and_start()
            .await
            .expect("could not authenticate and start up the gateway connection");

        gateway_client
    }

    // future responsible for periodically polling directory server and updating
    // the current global view of topology
    async fn start_topology_refresher(
        &mut self,
        topology_accessor: TopologyAccessor,
        shutdown: ShutdownListener,
    ) -> Result<(), ClientError> {
        let topology_refresher_config = TopologyRefresherConfig::new(
            self.config.get_base().get_validator_api_endpoints(),
            self.config.get_base().get_topology_refresh_rate(),
            env!("CARGO_PKG_VERSION").to_string(),
        );
        let mut topology_refresher =
            TopologyRefresher::new(topology_refresher_config, topology_accessor);
        // before returning, block entire runtime to refresh the current network view so that any
        // components depending on topology would see a non-empty view
        info!("Obtaining initial network topology");
        topology_refresher.refresh().await;

        // TODO: a slightly more graceful termination here
        if !topology_refresher.is_topology_routable().await {
            log::error!(
                "The current network topology seem to be insufficient to route any packets through \
                - check if enough nodes and a gateway are online"
            );
            return Err(ClientCoreError::InsufficientNetworkTopology.into());
        }

        info!("Starting topology refresher...");
        topology_refresher.start_with_shutdown(shutdown);
        Ok(())
    }

    // controller for sending sphinx packets to mixnet (either real traffic or cover traffic)
    // TODO: if we want to send control messages to gateway_client, this CAN'T take the ownership
    // over it. Perhaps GatewayClient needs to be thread-shareable or have some channel for
    // requests?
    fn start_mix_traffic_controller(
        gateway_client: GatewayClient,
        shutdown: ShutdownListener,
    ) -> BatchMixMessageSender {
        info!("Starting mix traffic controller...");
        let (mix_traffic_controller, mix_tx) = MixTrafficController::new(gateway_client);
        mix_traffic_controller.start_with_shutdown(shutdown);
        mix_tx
    }

    fn start_websocket_listener(
        &self,
        buffer_requester: ReceivedBufferRequestSender,
        msg_input: InputMessageSender,
    ) {
        info!("Starting websocket listener...");

        let websocket_handler =
            websocket::Handler::new(msg_input, buffer_requester, self.as_mix_recipient());

        websocket::Listener::new(self.config.get_listening_port()).start(websocket_handler);
    }

    /// EXPERIMENTAL DIRECT RUST API
    /// It's untested and there are absolutely no guarantees about it (but seems to have worked
    /// well enough in local tests)
    pub fn send_message(&mut self, recipient: Recipient, message: Vec<u8>, with_reply_surb: bool) {
        let input_msg = InputMessage::new_fresh(recipient, message, with_reply_surb);

        self.input_tx
            .as_ref()
            .expect("start method was not called before!")
            .unbounded_send(input_msg)
            .unwrap();
    }

    /// EXPERIMENTAL DIRECT RUST API
    /// It's untested and there are absolutely no guarantees about it (but seems to have worked
    /// well enough in local tests)
    pub fn send_reply(&mut self, reply_surb: ReplySurb, message: Vec<u8>) {
        let input_msg = InputMessage::new_reply(reply_surb, message);

        self.input_tx
            .as_ref()
            .expect("start method was not called before!")
            .unbounded_send(input_msg)
            .unwrap();
    }

    /// EXPERIMENTAL DIRECT RUST API
    /// It's untested and there are absolutely no guarantees about it (but seems to have worked
    /// well enough in local tests)
    /// Note: it waits for the first occurrence of messages being sent to ourselves. If you expect multiple
    /// messages, you might have to call this function repeatedly.
    // TODO: I guess this should really return something that `impl Stream<Item=ReconstructedMessage>`
    pub async fn wait_for_messages(&mut self) -> Vec<ReconstructedMessage> {
        use futures::StreamExt;

        self.receive_tx
            .as_mut()
            .expect("start method was not called before!")
            .next()
            .await
            .expect("buffer controller seems to have somehow died!")
    }

    /// blocking version of `start` method. Will run forever (or until SIGINT is sent)
    pub async fn run_forever(&mut self) -> Result<(), ClientError> {
        let shutdown = self.start().await?;
        wait_for_signal().await;

        println!(
            "Received signal - the client will terminate now (threads are not yet nicely stopped, if you see stack traces that's alright)."
        );

        log::info!("Sending shutdown");
        shutdown.signal_shutdown().ok();

        // Some of these components have shutdown signalling implemented as part of socks5 work,
        // but since it's not fully implemented (yet) for all the components of the native client,
        // we don't try to wait and instead just stop immediately.
        //log::info!("Waiting for tasks to finish... (Press ctrl-c to force)");
        //shutdown.wait_for_shutdown().await;

        log::info!("Stopping nym-client");
        Ok(())
    }

    pub async fn start(&mut self) -> Result<ShutdownNotifier, ClientError> {
        info!("Starting nym client");
        // channels for inter-component communication
        // TODO: make the channels be internally created by the relevant components
        // rather than creating them here, so say for example the buffer controller would create the request channels
        // and would allow anyone to clone the sender channel

        // unwrapped_sphinx_sender is the transmitter of mixnet messages received from the gateway
        // unwrapped_sphinx_receiver is the receiver for said messages - used by ReceivedMessagesBuffer
        let (mixnet_messages_sender, mixnet_messages_receiver) = mpsc::unbounded();

        // used for announcing connection or disconnection of a channel for pushing re-assembled messages to
        let (received_buffer_request_sender, received_buffer_request_receiver) = mpsc::unbounded();

        // channels responsible for controlling real messages
        let (input_sender, input_receiver) = mpsc::unbounded::<InputMessage>();

        // channels responsible for controlling ack messages
        let (ack_sender, ack_receiver) = mpsc::unbounded();
        let shared_topology_accessor = TopologyAccessor::new();

        let reply_key_storage =
            ReplyKeyStorage::load(self.config.get_base().get_reply_encryption_key_store_path())
                .expect("Failed to load reply key storage!");

        // Shutdown notifier for signalling tasks to stop
        let shutdown = ShutdownNotifier::default();

        // the components are started in very specific order. Unless you know what you are doing,
        // do not change that.
        self.start_topology_refresher(shared_topology_accessor.clone(), shutdown.subscribe())
            .await?;
        self.start_received_messages_buffer_controller(
            received_buffer_request_receiver,
            mixnet_messages_receiver,
            reply_key_storage.clone(),
            shutdown.subscribe(),
        );

        let gateway_client = self
            .start_gateway_client(mixnet_messages_sender, ack_sender, shutdown.subscribe())
            .await;

        // The sphinx_message_sender is the transmitter for any component generating sphinx packets
        // that are to be sent to the mixnet. They are used by cover traffic stream and real
        // traffic stream.
        // The MixTrafficController then sends the actual traffic
        let sphinx_message_sender =
            Self::start_mix_traffic_controller(gateway_client, shutdown.subscribe());

        self.start_real_traffic_controller(
            shared_topology_accessor.clone(),
            reply_key_storage,
            ack_receiver,
            input_receiver,
            sphinx_message_sender.clone(),
            shutdown.subscribe(),
        );

        if !self
            .config
            .get_base()
            .get_disabled_loop_cover_traffic_stream()
        {
            self.start_cover_traffic_stream(
                shared_topology_accessor,
                sphinx_message_sender,
                shutdown.subscribe(),
            );
        }

        match self.config.get_socket_type() {
            SocketType::WebSocket => {
                self.start_websocket_listener(received_buffer_request_sender, input_sender)
            }
            SocketType::None => {
                // if we did not start the socket, it means we're running (supposedly) in the native mode
                // and hence we should announce 'ourselves' to the buffer
                let (reconstructed_sender, reconstructed_receiver) = mpsc::unbounded();

                // tell the buffer to start sending stuff to us
                received_buffer_request_sender
                    .unbounded_send(ReceivedBufferMessage::ReceiverAnnounce(
                        reconstructed_sender,
                    ))
                    .expect("the buffer request failed!");

                self.receive_tx = Some(reconstructed_receiver);
                self.input_tx = Some(input_sender);
            }
        }

        info!("Client startup finished!");
        info!("The address of this client is: {}", self.as_mix_recipient());

        Ok(shutdown)
    }
}
