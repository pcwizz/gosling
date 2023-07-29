// standard
use std::clone::Clone;
use std::convert::TryInto;
use std::net::TcpStream;

// extern crates
use bson::doc;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson};
use honk_rpc::honk_rpc::{ApiSet, ErrorCode, RequestCookie, Session};
use rand::rngs::OsRng;
use rand::RngCore;
use tor_interface::tor_crypto::*;

// internal crates
use crate::ascii_string::*;
use crate::gosling::*;

//
// Endpoint Server
//

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("HonkRPC method failed: {0}")]
    HonkRPCFailure(#[from] honk_rpc::honk_rpc::Error),

    #[error("server is in invalid state: {0}")]
    InvalidState(String),

    #[error("incorrect usage: {0}")]
    IncorrectUsage(String),
}

pub(crate) enum EndpointServerEvent {
    ChannelRequestReceived {
        requested_channel: AsciiString,
    },
    // endpoint server has acepted incoming channel request from identity client
    HandshakeCompleted {
        client_service_id: V3OnionServiceId,
        channel_name: AsciiString,
    },
    // endpoint server has reject an incoming channel request
    HandshakeRejected {
        client_allowed: bool,
        client_requested_channel_valid: bool,
        client_proof_signature_valid: bool,
    },
}

#[derive(Debug, PartialEq)]
enum EndpointServerState {
    WaitingForBeginHandshake,
    ValidatingChannelRequest,
    ChannelRequestValidated,
    WaitingForSendResponse,
    HandledSendResponse,
    HandshakeComplete,
}

pub(crate) struct EndpointServer {
    // Session Data
    rpc: Option<Session<TcpStream, TcpStream>>,
    pub server_identity: V3OnionServiceId,
    allowed_client_identity: V3OnionServiceId,

    // State Machine Data
    state: EndpointServerState,
    begin_handshake_request_cookie: Option<RequestCookie>,
    client_identity: Option<V3OnionServiceId>,
    requested_channel: Option<AsciiString>,
    server_cookie: Option<ServerCookie>,
    handshake_succeeded: Option<bool>,

    // Verification flags

    // Client not on the block-list
    client_allowed: bool,
    // The requested endpoint is valid
    client_requested_channel_valid: bool,
    // The client proof is valid and signed with client's public key
    client_proof_signature_valid: bool,
}

impl EndpointServer
{
    fn get_state(&self) -> String {
        format!("{{ state: {:?}, begin_handshake_request_cookie: {:?}, client_identity: {:?}, requested_channel: {:?}, server_cookie: {:?}, handshake_succeeded:{:?} }}", self.state, self.begin_handshake_request_cookie, self.client_identity, self.requested_channel, self.server_cookie, self.handshake_succeeded)
    }

    pub fn new(
        rpc: Session<TcpStream, TcpStream>,
        client_identity: V3OnionServiceId,
        server_identity: V3OnionServiceId,
    ) -> Self {
        // generate server cookie
        let mut server_cookie: ServerCookie = Default::default();
        OsRng.fill_bytes(&mut server_cookie);

        EndpointServer {
            rpc: Some(rpc),
            server_identity,
            allowed_client_identity: client_identity,
            state: EndpointServerState::WaitingForBeginHandshake,
            begin_handshake_request_cookie: None,
            requested_channel: None,
            client_identity: None,
            server_cookie: None,
            handshake_succeeded: None,
            client_allowed: false,
            // TODO: hookup this to event and callback
            client_requested_channel_valid: true,
            client_proof_signature_valid: false,
        }
    }

    pub fn update(&mut self) -> Result<Option<EndpointServerEvent>, Error> {
        if let Some(mut rpc) = std::mem::take(&mut self.rpc) {
            rpc.update(Some(&mut [self])).unwrap();
            self.rpc = Some(rpc);
        }

        match(&self.state,
              self.begin_handshake_request_cookie,
              self.client_identity.as_ref(),
              self.requested_channel.as_ref(),
              self.server_cookie.as_ref(),
              self.handshake_succeeded) {
            (&EndpointServerState::WaitingForBeginHandshake,
             None, // begin_handshake_request_cookie
             None, // client_identity
             None, // requested_channel
             None, // server_cookie
             None) // handshake_succeeded
            => {},
            (&EndpointServerState::WaitingForBeginHandshake,
             Some(_begin_handshake_request_cookie),
             Some(_client_identity),
             Some(requested_channel),
             None, // server_cookie
             None) // handshake_succeeded
            => {
                self.state = EndpointServerState::ValidatingChannelRequest;
                return Ok(
                        Some(
                            EndpointServerEvent::ChannelRequestReceived
                            {
                                requested_channel: requested_channel.clone()
                            }));
            },
            (&EndpointServerState::ValidatingChannelRequest,
             Some(_begin_handshake_request_cookie),
             Some(_client_identity),
             Some(_requested_channel),
             None, // server_cookie
             None) // handshake_succeeded
            => {},
            (&EndpointServerState::ChannelRequestValidated,
             Some(_begin_handshake_request_cookie),
             Some(_client_identity),
             Some(_requested_channel),
             Some(_server_cookie),
             None) // handshake_succeeded
            => {},
            (&EndpointServerState::WaitingForSendResponse,
             Some(_begin_handshake_request_cookie),
             Some(_client_identity),
             Some(_requested_channel),
             Some(_server_cookie),
             None) // handshake_succeeded
            => {},
            (&EndpointServerState::HandledSendResponse,
             Some(_begin_handshake_request_cookie),
             Some(client_identity),
             Some(requested_channel),
             Some(_server_cookie),
             Some(handshake_succeeded))
            => {
                self.state = EndpointServerState::HandshakeComplete;
                if handshake_succeeded {
                    return Ok(Some(EndpointServerEvent::HandshakeCompleted{
                        client_service_id: client_identity.clone(),
                        channel_name: requested_channel.clone()}));
                } else {
                    return Ok(Some(EndpointServerEvent::HandshakeRejected{
                        client_allowed: self.client_allowed,
                        client_requested_channel_valid: self.client_requested_channel_valid,
                        client_proof_signature_valid: self.client_proof_signature_valid}));
                }
            },
            _ => return Err(Error::InvalidState(self.get_state())),
        }

        Ok(None)
    }

    // internal use
    fn handle_begin_handshake(
        &mut self,
        version: String,
        channel_name: AsciiString,
    ) -> Result<(), RpcError> {
        if version != GOSLING_VERSION {
            Err(RpcError::BadVersion)
        } else {
            self.requested_channel = Some(channel_name);
            Ok(())
        }
    }

    pub fn handle_channel_request_received(
        &mut self,
        client_requested_channel_valid: bool,
    ) -> Result<(), Error> {
        match(&self.state,
              self.begin_handshake_request_cookie,
              self.client_identity.as_ref(),
              self.requested_channel.as_ref(),
              self.server_cookie.as_ref(),
              self.handshake_succeeded) {
            (&EndpointServerState::ValidatingChannelRequest,
             Some(_begin_handshake_request_cookie),
             Some(client_identity),
             Some(_requested_channel),
             None, // server_cookie
             None) // handshake_succeeded
            => {
                let mut server_cookie: ServerCookie = Default::default();
                OsRng.fill_bytes(&mut server_cookie);
                self.server_cookie = Some(server_cookie);
                self.client_allowed = *client_identity == self.allowed_client_identity;
                self.client_requested_channel_valid = client_requested_channel_valid;
                self.state = EndpointServerState::ChannelRequestValidated;
                Ok(())
            },
            _ => Err(Error::IncorrectUsage("handle_channel_request_received() may only be called after ChannelRequestReceived has been returned from update(), and it may only be called once".to_string()))
        }
    }

    // internal use
    fn handle_send_response(
        &mut self,
        client_cookie: ClientCookie,
        client_identity: V3OnionServiceId,
        client_identity_proof_signature: Ed25519Signature,
    ) -> Result<bson::Bson, RpcError> {
        // convert client_identity to client's public ed25519 key
        if let (Ok(client_identity_key), Some(requested_channel)) = (
            Ed25519PublicKey::from_service_id(&client_identity),
            self.requested_channel.as_ref(),
        ) {
            let server_cookie = match self.server_cookie.as_ref() {
                Some(server_cookie) => server_cookie,
                None => unreachable!(),
            };

            // construct + verify client proof
            let client_proof = build_client_proof(
                DomainSeparator::GoslingEndpoint,
                requested_channel,
                &client_identity,
                &self.server_identity,
                &client_cookie,
                server_cookie,
            );
            self.client_proof_signature_valid =
                client_identity_proof_signature.verify(&client_proof, &client_identity_key);

            if self.client_allowed
                && self.client_requested_channel_valid
                && self.client_proof_signature_valid
            {
                self.handshake_succeeded = Some(true);
                self.state = EndpointServerState::HandledSendResponse;
                // success, return empty doc
                return Ok(Bson::Document(doc! {}));
            }
        }
        self.handshake_succeeded = Some(false);
        self.state = EndpointServerState::HandledSendResponse;
        Err(RpcError::Failure)
    }
}

impl ApiSet for EndpointServer
{
    fn namespace(&self) -> &str {
        "gosling_endpoint"
    }

    fn exec_function(
        &mut self,
        name: &str,
        version: i32,
        mut args: bson::document::Document,
        request_cookie: Option<RequestCookie>,
    ) -> Result<Option<bson::Bson>, ErrorCode> {
        let request_cookie = match request_cookie {
            Some(request_cookie) => request_cookie,
            None => return Err(ErrorCode::Runtime(RpcError::RequestCookieRequired as i32)),
        };

        match
            (name, version,
             &self.state,
             self.client_identity.as_ref(),
             self.requested_channel.as_ref()) {
            // handle begin_handshake call
            ("begin_handshake", 0,
            &EndpointServerState::WaitingForBeginHandshake,
            None, // client_identity
            None) // requested_channel
            => {
                if let (Some(Bson::String(version)),
                        Some(Bson::String(client_identity)),
                        Some(Bson::String(channel_name))) =
                       (args.remove("version"),
                        args.remove("client_identity"),
                        args.remove("channel")) {
                    self.begin_handshake_request_cookie = Some(request_cookie);

                    // client_identiity
                    self.client_identity = match V3OnionServiceId::from_string(&client_identity) {
                        Ok(client_identity) => Some(client_identity),
                        Err(_) => return Err(ErrorCode::Runtime(RpcError::InvalidArg as i32)),
                    };

                    let channel_name = match AsciiString::new(channel_name) {
                        Ok(channel_name) => channel_name,
                        Err(_) => return Err(ErrorCode::Runtime(RpcError::InvalidArg as i32)),
                    };

                    match self.handle_begin_handshake(version, channel_name) {
                        Ok(()) => Ok(None),
                        Err(err) => Err(ErrorCode::Runtime(err as i32)),
                    }
                } else {
                    Err(ErrorCode::Runtime(RpcError::InvalidArg as i32))
                }
            },
            ("send_response", 0,
            &EndpointServerState::WaitingForSendResponse,
            Some(client_identity),
            Some(_requested_channel))
            => {
                if let (Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: client_cookie})),
                        Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: client_identity_proof_signature}))) =
                       (args.remove("client_cookie"),
                        args.remove("client_identity_proof_signature")) {
                    // client_cookie
                    let client_cookie : ClientCookie = match client_cookie.try_into() {
                        Ok(client_cookie) => client_cookie,
                        Err(_) => return Err(ErrorCode::Runtime(RpcError::InvalidArg as i32)),
                    };

                    // client_identity_proof_signature
                    let client_identity_proof_signature : [u8; ED25519_SIGNATURE_SIZE] = match client_identity_proof_signature.try_into() {
                        Ok(client_identity_proof_signature) => client_identity_proof_signature,
                        Err(_) => return Err(ErrorCode::Runtime(RpcError::InvalidArg as i32)),
                    };

                    let client_identity_proof_signature = match Ed25519Signature::from_raw(&client_identity_proof_signature) {
                        Ok(client_identity_proof_signature) => client_identity_proof_signature,
                        Err(_) => return Err(ErrorCode::Runtime(RpcError::InvalidArg as i32)),
                    };

                    match self.handle_send_response(client_cookie, client_identity.clone(), client_identity_proof_signature) {
                        Ok(result) => Ok(Some(result)),
                        Err(err) => Err(ErrorCode::Runtime(err as i32)),
                    }
                } else {
                    Err(ErrorCode::Runtime(RpcError::InvalidArg as i32))
                }
            },
            _ => Ok(None),
        }
    }

    fn next_result(&mut self) -> Option<(RequestCookie, Option<bson::Bson>, ErrorCode)> {
        match (
            &self.state,
            self.begin_handshake_request_cookie,
            self.server_cookie.as_ref(),
        ) {
            (
                &EndpointServerState::ChannelRequestValidated,
                Some(begin_handshake_request_cookie),
                Some(server_cookie),
            ) => {
                self.state = EndpointServerState::WaitingForSendResponse;
                Some((
                    begin_handshake_request_cookie,
                    Some(Bson::Document(doc! {
                        "server_cookie" : Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: server_cookie.to_vec()}),
                    })),
                    ErrorCode::Success,
                ))
            }
            _ => None,
        }
    }
}
