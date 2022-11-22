// standard
use std::clone::Clone;
use std::collections::{BTreeMap,HashMap};
use std::convert::TryInto;
#[cfg(test)]
use std::io::{BufRead,BufReader,Write};
use std::net::TcpStream;
use std::path::Path;

// extern crates

use bson::doc;
use bson::{Binary,Bson};
use bson::spec::BinarySubtype;
use data_encoding::{HEXLOWER};
use num_enum::TryFromPrimitive;
use rand::RngCore;
use rand::rngs::OsRng;
#[cfg(test)]
use serial_test::serial;

// internal crates
use crate::*;
use crate::error::Result;
use crate::honk_rpc::*;
#[cfg(test)]
use crate::test_utils::MemoryStream;
use crate::tor_crypto::*;
use crate::tor_controller::*;

#[derive(Debug, Eq, PartialEq, TryFromPrimitive)]
#[repr(i32)]
/// cbindgen:ignore
enum GoslingError {
    // bad gosling version
    BadVersion,
    // cookie required
    RequestCookieRequired,
    // invalid or missing arguments
    InvalidArg,
    // generic runtime error
    Failure,
}

const GOSLING_VERSION: &str = "0.0.0.1";

const CLIENT_COOKIE_SIZE: usize = 32usize;
const SERVER_COOKIE_SIZE: usize = 32usize;

type ClientCookie = [u8; CLIENT_COOKIE_SIZE];
type ServerCookie = [u8; SERVER_COOKIE_SIZE];
type ClientProof = Vec<u8>;

enum DomainSeparator {
    GoslingIdentity,
    GoslingEndpoint,
}

impl From<DomainSeparator> for &[u8] {
    fn from(sep: DomainSeparator) -> &'static [u8] {
        match sep {
            DomainSeparator::GoslingIdentity => b"gosling-identity",
            DomainSeparator::GoslingEndpoint => b"gosling-endpoint",
        }
    }
}

fn build_client_proof(domain_separator: DomainSeparator,
                      request: &str,
                      client_service_id: &V3OnionServiceId,
                      server_service_id: &V3OnionServiceId,
                      client_cookie: &ClientCookie,
                      server_cookie: &ServerCookie) -> Result<ClientProof> {
    ensure!(request.is_ascii());

    let mut client_proof : ClientProof = Default::default();

    client_proof.extend_from_slice(domain_separator.into());
    client_proof.push(0u8);
    client_proof.extend_from_slice(request.as_bytes());
    client_proof.push(0u8);
    client_proof.extend_from_slice(client_service_id.to_string().as_bytes());
    client_proof.push(0u8);
    client_proof.extend_from_slice(server_service_id.to_string().as_bytes());
    client_proof.push(0u8);
    client_proof.extend_from_slice(HEXLOWER.encode(client_cookie).as_bytes());
    client_proof.push(0u8);
    client_proof.extend_from_slice(HEXLOWER.encode(server_cookie).as_bytes());

    Ok(client_proof)
}

//
// Identity Client
//

enum IdentityClientEvent {
    ChallengeReceived {
        identity_service_id: V3OnionServiceId,
        endpoint_name: String,
        endpoint_challenge: bson::document::Document,
    },
    HandshakeCompleted {
        identity_service_id: V3OnionServiceId,
        endpoint_service_id: V3OnionServiceId,
        endpoint_name: String,
        client_auth_private_key: X25519PrivateKey,
    },
}

#[derive(Debug, PartialEq)]
enum IdentityClientState {
    BeginHandshake,
    WaitingForChallenge,
    WaitingForChallengeResponse,
    WaitingForChallengeVerification,
    HandshakeComplete,
}

//
// An identity client object used for connecting
// to an identity server
//
struct IdentityClient<RW> {
    // session data
    rpc: Session<RW,RW>,
    server_service_id: V3OnionServiceId,
    requested_endpoint: String,
    client_service_id: V3OnionServiceId,
    client_ed25519_private: Ed25519PrivateKey,
    client_x25519_private: X25519PrivateKey,

    // state machine data
    state: IdentityClientState,
    begin_handshake_request_cookie: Option<RequestCookie>,
    server_cookie: Option<ServerCookie>,
    endpoint_challenge_response: Option<bson::document::Document>,
    send_response_request_cookie: Option<RequestCookie>,

}

impl<RW> IdentityClient<RW> where RW : std::io::Read + std::io::Write + Send {
    fn new(
        rpc: Session<RW,RW>,
        server_service_id: V3OnionServiceId,
        requested_endpoint: String,
        client_ed25519_private: Ed25519PrivateKey,
        client_x25519_private: X25519PrivateKey) -> Self {
        Self {
            rpc,
            server_service_id,
            requested_endpoint,
            client_service_id: V3OnionServiceId::from_private_key(&client_ed25519_private),
            client_ed25519_private,
            client_x25519_private,

            state: IdentityClientState::BeginHandshake,
            begin_handshake_request_cookie: None,
            server_cookie: None,
            send_response_request_cookie: None,
            endpoint_challenge_response: None,
        }
    }

    fn update(&mut self) -> Result<Option<IdentityClientEvent>> {

        ensure!(self.state != IdentityClientState::HandshakeComplete);

        // update our rpc session
        self.rpc.update(None)?;

        // client state machine
        match (&self.state, self.begin_handshake_request_cookie, self.server_cookie,  self.endpoint_challenge_response.take(), self.send_response_request_cookie) {
            // send initial handshake request
            (&IdentityClientState::BeginHandshake, None, None, None, None) => {
                self.begin_handshake_request_cookie = Some(self.rpc.client_call(
                    "gosling_identity",
                    "begin_handshake",
                    0,
                    doc!{
                        "version" : bson::Bson::String(GOSLING_VERSION.to_string()),
                        "client_identity" : bson::Bson::String(self.client_service_id.to_string()),
                        "endpoint" : bson::Bson::String(self.requested_endpoint.clone()),
                    })?);
                self.state = IdentityClientState::WaitingForChallenge;
            },
            (&IdentityClientState::WaitingForChallenge, Some(begin_handshake_request_cookie), None, None, None) => {
                if let Some(response) = self.rpc.client_next_response() {
                    // check for response for the begin_handshake() call
                    let mut response = match response {
                        Response::Pending{cookie} => {
                            ensure!(cookie == begin_handshake_request_cookie, "received unexpected pending response");
                            return Ok(None);
                        },
                        Response::Error{cookie, error_code} => {
                            ensure!(cookie == begin_handshake_request_cookie, "received unexpected error response; rpc error_code: {}", error_code);
                            bail!("rpc error_code: {}", error_code);
                        },
                        Response::Success{cookie, result} => {
                            ensure!(cookie == begin_handshake_request_cookie, "received unexpected success response");
                            match result {
                                Bson::Document(result) => result,
                                _ => bail!("received unexpected bson type"),
                            }
                        },
                    };

                    // save off the server cookie
                    self.server_cookie = match response.get("server_cookie"){
                        Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: server_cookie})) => match server_cookie.clone().try_into() {
                                Ok(server_cookie) => Some(server_cookie),
                                Err(_) => bail!(""),
                            },
                        Some(_) => bail!("server_cookie is unxpected bson type"),
                        None => bail!("missing server_cookie"),
                    };

                    // get the endpoint challenge
                    let endpoint_challenge = match response.get_mut("endpoint_challenge") {
                        Some(Bson::Document(endpoint_challenge)) => std::mem::take(endpoint_challenge),
                        Some(_) => bail!("endpoint challenge is unexpected bson type"),
                        None => bail!("missing endpoint_challenge"),
                    };

                    self.state = IdentityClientState::WaitingForChallengeResponse;
                    return Ok(Some(IdentityClientEvent::ChallengeReceived{
                        identity_service_id: self.server_service_id.clone(),
                        endpoint_name: self.requested_endpoint.clone(),
                        endpoint_challenge,
                    }));
                }
            },
            (&IdentityClientState::WaitingForChallengeResponse, Some(_begin_handshake_request_cookie), Some(_server_cookie), None, None) => {
                return  Ok(None);
            },
            (&IdentityClientState::WaitingForChallengeResponse, Some(_begin_handshake_request_cookie), Some(server_cookie), Some(endpoint_challenge_response), None) => {
                // client_cookie
                let mut client_cookie: ClientCookie = Default::default();
                OsRng.fill_bytes(&mut client_cookie);
                let client_cookie = client_cookie;

                // client_identity_proof_signature
                let client_identity_proof = build_client_proof(
                    DomainSeparator::GoslingIdentity,
                    &self.requested_endpoint,
                    &self.client_service_id,
                    &self.server_service_id,
                    &client_cookie,
                    &server_cookie,
                )?;
                let client_identity_proof_signature = self.client_ed25519_private.sign_message(&client_identity_proof);

                // client_authorization_key
                 let client_authorization_key = X25519PublicKey::from_private_key(&self.client_x25519_private);

                // client_authorization_signature
                let client_identity = self.client_service_id.to_string();
                let (client_authorization_signature, signbit) = self.client_x25519_private.sign_message(client_identity.as_bytes())?;

                // client_authorization_key_signbit
                let client_authorization_key_signbit = match signbit {
                    0u8 => false,
                    1u8 => true,
                    _ => bail!("invalid signbit"),
                };

               // build our args object for rpc call
                let args = doc!{
                    "client_cookie" : bson::Bson::Binary(bson::Binary{subtype: BinarySubtype::Generic, bytes: client_cookie.to_vec()}),
                    "client_identity_proof_signature" : bson::Bson::Binary(bson::Binary{subtype: BinarySubtype::Generic, bytes: client_identity_proof_signature.to_bytes().to_vec()}),
                    "client_authorization_key" : bson::Bson::Binary(bson::Binary{subtype: BinarySubtype::Generic, bytes: client_authorization_key.as_bytes().to_vec()}),
                    "client_authorization_key_signbit" : bson::Bson::Boolean(client_authorization_key_signbit),
                    "client_authorization_signature" : bson::Bson::Binary(bson::Binary{subtype: BinarySubtype::Generic, bytes: client_authorization_signature.to_bytes().to_vec()}),
                    "challenge_response" : endpoint_challenge_response,
                };

                // make rpc call
                self.send_response_request_cookie = Some(self.rpc.client_call(
                    "gosling_identity",
                    "send_response",
                    0,
                    args)?);
                self.state = IdentityClientState::WaitingForChallengeVerification;
            },
            (&IdentityClientState::WaitingForChallengeVerification, Some(_begin_handshake_request_cookie), Some(_server_cookie), None, Some(send_response_request_cookie)) => {
                if let Some(response) = self.rpc.client_next_response() {
                    let endpoint_service_id = match response {
                        Response::Pending{cookie} => {
                            ensure!(cookie == send_response_request_cookie, "received unexpected pending response");
                            return Ok(None);
                        },
                        Response::Error{cookie, error_code} => {
                            ensure!(cookie == send_response_request_cookie, "received unexpected error response; rpc error_code: {}", error_code);
                            bail!("rpc error_code: {}", error_code);
                        },
                        Response::Success{cookie, result} => {
                            ensure!(cookie == send_response_request_cookie, "received unexpected success response");
                            match result {
                                Bson::String(endpoint_service_id) => V3OnionServiceId::from_string(&endpoint_service_id)?,
                                _ => bail!("received unexpected bson type"),
                            }
                        },
                    };
                    self.state = IdentityClientState::HandshakeComplete;
                    return Ok(Some(IdentityClientEvent::HandshakeCompleted{
                        identity_service_id: self.server_service_id.clone(),
                        endpoint_service_id,
                        endpoint_name: self.requested_endpoint.clone(),
                        client_auth_private_key: self.client_x25519_private.clone(),
                    }));
                }
            },
            _ => {
                bail!("unexpected state: state: {:?},  begin_handshake_request_cookie: {:?},  server_cookie: {:?}, endpoint_challenge_response: {:?},  send_response_request_cookie: {:?}", self.state,  self.begin_handshake_request_cookie, self.server_cookie, self.endpoint_challenge_response, self.send_response_request_cookie);
            },
        }
        Ok(None)
    }

    fn send_response(&mut self, challenge_response: bson::document::Document) -> Result<()> {
        ensure!(self.state == IdentityClientState::WaitingForChallengeResponse);
        self.endpoint_challenge_response = Some(challenge_response);
        Ok(())
    }
}

//
// Identity Server
//

enum IdentityServerEvent {
    EndpointRequestReceived{
        client_service_id: V3OnionServiceId,
        requested_endpoint: String,
    },

    ChallengeResponseReceived{
        challenge_response: bson::document::Document,
    },

    HandshakeCompleted{
        endpoint_private_key: Ed25519PrivateKey,
        endpoint_name: String,
        client_service_id: V3OnionServiceId,
        client_auth_public_key: X25519PublicKey,
    },

    HandshakeRejected{
        // Client not on the block-list
        client_allowed: bool,
        // The requested endpoint is valid
        client_requested_endpoint_valid: bool,
        // The client proof is valid and signed with client's public key
        client_proof_signature_valid: bool,
        // The client authorization signature is valid
        client_auth_signature_valid: bool,
        // The challenge response is valid
        challenge_response_valid: bool,
    },
}

#[derive(Debug, PartialEq)]
enum IdentityServerState {
    WaitingForBeginHandshake,
    GettingChallenge,
    ChallengeReady,
    WaitingForSendResponse,
    GettingChallengeVerification,
    ChallengeVerificationReady,
    ChallengeVerificationResponseSent,
    HandshakeComplete,
}

struct IdentityServer<RW> {
    // Session Data
    rpc: Option<Session<RW,RW>>,
    server_identity: V3OnionServiceId,

    // State Machine Data
    state: IdentityServerState,
    begin_handshake_request_cookie: Option<RequestCookie>,
    requested_endpoint: Option<String>,
    server_cookie: Option<ServerCookie>,
    endpoint_challenge: Option<bson::document::Document>,
    send_response_request_cookie: Option<RequestCookie>,
    client_identity: Option<V3OnionServiceId>,
    client_auth_key: Option<X25519PublicKey>,
    challenge_response: Option<bson::document::Document>,
    endpoint_private_key: Option<Ed25519PrivateKey>,

    // Verification flags

    // Client not on the block-list
    client_allowed: bool,
    // The requested endpoint is valid
    client_requested_endpoint_valid: bool,
    // The client proof is valid and signed with client's public key
    client_proof_signature_valid: bool,
    // The client authorization signature is valid
    client_auth_signature_valid: bool,
    // The challenge response is valid
    challenge_response_valid: bool,
}

impl<RW> IdentityServer<RW> where RW : std::io::Read + std::io::Write + Send {
    pub fn new(rpc: Session<RW,RW>, server_identity: V3OnionServiceId) -> Self {
        IdentityServer{
            // Session Data
            rpc: Some(rpc),
            server_identity,

            // State Machine Data
            state: IdentityServerState::WaitingForBeginHandshake,
            begin_handshake_request_cookie: None,
            client_identity: None,
            requested_endpoint: None,
            server_cookie: None,
            endpoint_challenge: None,
            send_response_request_cookie: None,
            client_auth_key: None,
            challenge_response: None,
            endpoint_private_key: None,

            // Verification Flags
            client_allowed: false,
            client_requested_endpoint_valid: false,
            client_proof_signature_valid: false,
            client_auth_signature_valid: false,
            challenge_response_valid: false,
        }
    }

    pub fn update(&mut self) -> Result<Option<IdentityServerEvent>> {
        // cursed or brilliant?
        if let Some(mut rpc) = std::mem::take(&mut self.rpc) {
            rpc.update(Some(&mut [self]))?;
            self.rpc = Some(rpc);
        }

        match(&self.state,
              self.begin_handshake_request_cookie,
              self.client_identity.as_ref(),
              self.requested_endpoint.as_ref(),
              self.server_cookie.as_ref(),
              self.endpoint_challenge.as_ref(),
              self.send_response_request_cookie,
              self.client_auth_key.as_ref(),
              self.challenge_response.as_mut(),
              self.endpoint_private_key.as_ref()) {
            (&IdentityServerState::WaitingForBeginHandshake,
             Some(_begin_handshake_request_cookie),
             Some(client_identity),
             Some(requested_endpoint),
             None, // server_cookie
             None, // endpoint_challenge
             None, // send_response_request_cookie
             None, // client_auth_key
             None, // challenge_response
             None) // endpoint_private_key
            => {
                self.state = IdentityServerState::GettingChallenge;
                return Ok(Some(IdentityServerEvent::EndpointRequestReceived{client_service_id: client_identity.clone(), requested_endpoint: requested_endpoint.clone()}));
            },
            (&IdentityServerState::WaitingForSendResponse,
             Some(_begin_handshake_request_cookie),
             Some(_client_identity),
             Some(_requested_endpoint),
             Some(_server_cookie),
             Some(_endpoint_challenge),
             Some(_send_response_request_cookie),
             Some(_client_auth_key),
             Some(challenge_response),
             None) // endpoint_private_key
            => {
                self.state = IdentityServerState::GettingChallengeVerification;
                return Ok(Some(IdentityServerEvent::ChallengeResponseReceived{
                    challenge_response: std::mem::take(challenge_response),
                }));
            },
            (&IdentityServerState::ChallengeVerificationResponseSent,
             Some(_begin_handshake_request_cookie),
             Some(client_identity),
             Some(requested_endpoint),
             Some(_server_cookie),
             Some(_endpoint_challenge),
             Some(_send_response_request_cookie),
             Some(client_auth_key),
             Some(_challenge_response),
             Some(endpoint_private_key))
            => {
                self.state = IdentityServerState::HandshakeComplete;
                return Ok(Some(IdentityServerEvent::HandshakeCompleted{
                    endpoint_private_key: endpoint_private_key.clone(),
                    endpoint_name: requested_endpoint.clone(),
                    client_service_id: client_identity.clone(),
                    client_auth_public_key: client_auth_key.clone(),
                }));
            },
            (&IdentityServerState::ChallengeVerificationResponseSent,
             Some(_begin_handshake_request_cookie),
             Some(_client_identity),
             Some(_requested_endpoint),
             Some(_server_cookie),
             Some(_endpoint_challenge),
             Some(_send_response_request_cookie),
             Some(_client_auth_key),
             Some(_challenge_response),
             None) // endpoint_private_key
            => {
                self.state = IdentityServerState::HandshakeComplete;
                return Ok(Some(IdentityServerEvent::HandshakeRejected{
                    client_allowed: self.client_allowed,
                    client_requested_endpoint_valid: self.client_requested_endpoint_valid,
                    client_proof_signature_valid: self.client_proof_signature_valid,
                    client_auth_signature_valid: self.client_auth_signature_valid,
                    challenge_response_valid: self.challenge_response_valid,
                }));
            },
            _ => {},
        }

        Ok(None)
    }

    // internal use
    fn handle_begin_handshake(
        &mut self,
        version: String,
        endpoint_name: String) -> Result<(), GoslingError> {

        if version != GOSLING_VERSION {
            Err(GoslingError::BadVersion)
        } else {
            self.requested_endpoint = Some(endpoint_name);
            Ok(())
        }
    }

    pub fn send_challenge(
        &mut self,
        client_allowed: bool,
        endpoint_valid: bool,
        endpoint_challenge: bson::document::Document) -> Result<(), error::Error> {

        match (&self.state,
               self.begin_handshake_request_cookie,
               self.client_identity.as_ref(),
               self.requested_endpoint.as_ref(),
               self.server_cookie.as_ref(),
               self.endpoint_challenge.as_ref(),
               self.client_auth_key.as_ref(),
               self.challenge_response.as_ref(),
               self.endpoint_private_key.as_ref()) {
              (&IdentityServerState::GettingChallenge,
               Some(_begin_handshake_request_cookie),
               Some(_client_identity),
               Some(_endpoint_name),
               None, // server_cookie
               None, // endpoint_challenge
               None, // client_auth_key
               None, // challenge_response
               None) => // endpoint_private_key
             {
                let mut server_cookie: ServerCookie = Default::default();
                OsRng.fill_bytes(&mut server_cookie);
                self.server_cookie = Some(server_cookie);
                self.endpoint_challenge = Some(endpoint_challenge);
                self.client_allowed = client_allowed;
                self.client_requested_endpoint_valid = endpoint_valid;
                self.state = IdentityServerState::ChallengeReady;
                Ok(())
            },
            _ => {
                bail!("unexpected state: state: {:?}", self.state);
            },
        }
    }

    // internal use
    fn handle_send_response(
        &mut self,
        client_cookie: ClientCookie,
        client_identity: V3OnionServiceId,
        client_identity_proof_signature: Ed25519Signature,
        client_authorization_key: X25519PublicKey,
        client_authorization_key_signbit: u8,
        client_authorization_signature: Ed25519Signature,
        challenge_response: bson::document::Document) -> Result<(), GoslingError> {

        // convert client_identity to client's public ed25519 key
        if let Ok(client_identity_key) = Ed25519PublicKey::from_service_id(&client_identity) {
            // construct + verify client proof
            if let Ok(client_proof) = build_client_proof(
                                            DomainSeparator::GoslingIdentity,
                                            self.requested_endpoint.as_ref().unwrap(),
                                            &client_identity,
                                            &self.server_identity,
                                            &client_cookie,
                                            self.server_cookie.as_ref().unwrap()) {
                self.client_proof_signature_valid = client_identity_proof_signature.verify(&client_proof, &client_identity_key);
            };
        }

        // evaluate the client authorization signature
        self.client_auth_signature_valid = client_authorization_signature.verify_x25519(client_identity.as_bytes(), &client_authorization_key, client_authorization_key_signbit);

        // save off client auth key for future endpoint generation
        self.client_auth_key = Some(client_authorization_key);

        // safe off challenge response for verification
        self.challenge_response = Some(challenge_response);
        Ok(())
    }

    pub fn send_challenge_verification(
        &mut self,
        challenge_response_valid: bool) -> Result<(), error::Error> {

        match (&self.state,
               self.begin_handshake_request_cookie,
               self.client_identity.as_ref(),
               self.requested_endpoint.as_ref(),
               self.server_cookie.as_ref(),
               self.endpoint_challenge.as_ref(),
               self.client_auth_key.as_ref(),
               self.challenge_response.as_ref(),
               self.endpoint_private_key.as_ref()) {
              (&IdentityServerState::GettingChallengeVerification,
               Some(_begin_handshake_request_cookie),
               Some(_client_identity),
               Some(_requested_endpoint),
               Some(_server_cookie),
               Some(_endpoint_challenge),
               Some(_client_auth_key),
               Some(_challenge_response),
               None) => // endpoint_private_key
            {
                self.challenge_response_valid = challenge_response_valid;
                self.state = IdentityServerState::ChallengeVerificationReady;
            },
            _ => { bail!("unexpected state"); }
        }

        Ok(())
    }
}

impl<RW> ApiSet for IdentityServer<RW> where RW : std::io::Read + std::io::Write + Send {
    fn namespace(&self) -> &str {
        "gosling_identity"
    }

    fn exec_function(&mut self, name: &str, version: i32, mut args: bson::document::Document, request_cookie: Option<RequestCookie>) -> Result<Option<bson::Bson>, ErrorCode> {

        let request_cookie = match request_cookie {
            Some(request_cookie) => request_cookie,
            None => return Err(ErrorCode::Runtime(GoslingError::RequestCookieRequired as i32)),
        };

        match (name, version,
               &self.state,
               self.begin_handshake_request_cookie,
               self.client_identity.as_ref(),
               self.requested_endpoint.as_ref(),
               self.server_cookie.as_ref(),
               self.endpoint_challenge.as_ref(),
               self.client_auth_key.as_ref(),
               self.challenge_response.as_ref(),
               self.endpoint_private_key.as_ref()) {
            // handle begin_handshake call
            ("begin_handshake", 0,
             &IdentityServerState::WaitingForBeginHandshake,
             None, // begin_handshake_request_cookie
             None, // client_identity
             None, // requested_endpoint
             None, // server_cookie
             None, // endpoint_challenge
             None, // client_auth_key
             None, // challenge_response
             None) => // endpoint_private_key
            {
                if let (Some(Bson::String(version)),
                        Some(Bson::String(client_identity)),
                        Some(Bson::String(endpoint_name))) =
                       (args.remove("version"),
                        args.remove("client_identity"),
                        args.remove("endpoint")) {
                    self.begin_handshake_request_cookie = Some(request_cookie);

                    // client_identiity
                    self.client_identity = match V3OnionServiceId::from_string(&client_identity) {
                        Ok(client_identity) => Some(client_identity),
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    match self.handle_begin_handshake(version, endpoint_name) {
                        Ok(()) => Ok(None),
                        Err(err) => Err(ErrorCode::Runtime(err as i32)),
                    }
                } else {
                    Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32))
                }
            },
            // handle send_response call
            ("send_response", 0,
             &IdentityServerState::WaitingForSendResponse,
             Some(_begin_handshake_request_cookie),
             Some(client_identity),
             Some(_endpoint_name),
             Some(_server_cookie),
             Some(_endpoint_challenge),
             None, // client_auth_key
             None, // challenge_response
             None) => // endpoint_private_key
            {
                // arg validation
                if let (Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: client_cookie})),
                        Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: client_identity_proof_signature})),
                        Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: client_authorization_key})),
                        Some(Bson::Boolean(client_authorization_key_signbit)),
                        Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: client_authorization_signature})),
                        Some(Bson::Document(challenge_response))) =
                       (args.remove("client_cookie"),
                        args.remove("client_identity_proof_signature"),
                        args.remove("client_authorization_key"),
                        args.remove("client_authorization_key_signbit"),
                        args.remove("client_authorization_signature"),
                        args.remove("challenge_response")) {
                    self.send_response_request_cookie = Some(request_cookie);

                    // client_cookie
                    let client_cookie : ClientCookie = match client_cookie.try_into() {
                        Ok(client_cookie) => client_cookie,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    // client_identity_proof_signature
                    let client_identity_proof_signature : [u8; ED25519_SIGNATURE_SIZE] = match client_identity_proof_signature.try_into() {
                        Ok(client_identity_proof_signature) => client_identity_proof_signature,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    let client_identity_proof_signature = match Ed25519Signature::from_raw(&client_identity_proof_signature) {
                        Ok(client_identity_proof_signature) => client_identity_proof_signature,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    // client_authorization_key
                    let client_authorization_key : [u8; X25519_PUBLIC_KEY_SIZE] = match client_authorization_key.try_into() {
                        Ok(client_authorization_key) => client_authorization_key,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    let client_authorization_key = X25519PublicKey::from_raw(&client_authorization_key);

                    // client_authorization_key_signbit
                    let client_authorization_key_signbit = client_authorization_key_signbit as u8;

                    // client_authorization_signature
                    let client_authorization_signature : [u8; ED25519_SIGNATURE_SIZE] = match client_authorization_signature.try_into() {
                        Ok(client_authorization_signature) => client_authorization_signature,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    let client_authorization_signature = match Ed25519Signature::from_raw(&client_authorization_signature) {
                        Ok(client_authorization_signature) => client_authorization_signature,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };
                    match self.handle_send_response(
                            client_cookie,
                            client_identity.clone(),
                            client_identity_proof_signature,
                            client_authorization_key,
                            client_authorization_key_signbit,
                            client_authorization_signature,
                            challenge_response) {
                        Ok(()) => Ok(None),
                        Err(err) => Err(ErrorCode::Runtime(err as i32)),
                    }
                } else {
                    Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32))
                }
            },
            _ => Err(ErrorCode::Runtime(GoslingError::Failure as i32))
        }
    }

    fn next_result(&mut self) -> Option<(RequestCookie, Option<bson::Bson>, ErrorCode)> {
        match (&self.state,
               self.begin_handshake_request_cookie,
               self.client_identity.as_ref(),
               self.requested_endpoint.as_ref(),
               self.server_cookie.as_ref(),
               self.endpoint_challenge.as_mut(),
               self.send_response_request_cookie,
               self.client_auth_key.as_ref(),
               self.challenge_response.as_ref()) {
            // return challenge from begin_handshake
            (&IdentityServerState::ChallengeReady,
             Some(begin_handshake_request_cookie),
             Some(_client_identity),
             Some(_requested_endpoint),
             Some(server_cookie),
             Some(endpoint_challenge),
             None, // send_response_request_cookie
             None, // client_auth_key
             None) => // challenge_response
            {
                self.state = IdentityServerState::WaitingForSendResponse;
                Some((
                    begin_handshake_request_cookie,
                    Some(Bson::Document(doc!{
                        "server_cookie" : Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: server_cookie.to_vec()}),
                        "endpoint_challenge" : std::mem::take(endpoint_challenge),
                    })),
                    ErrorCode::Success))
            },
            (&IdentityServerState::ChallengeReady, _, _, _, _, _, _, _, _) => unreachable!(),
            (&IdentityServerState::ChallengeVerificationReady,
             Some(_begin_handshake_request_cookie),
             Some(_client_identity),
             Some(_requested_endpoint),
             Some(_server_cookie),
             Some(_endpoint_challenge),
             Some(send_response_request_cookie),
             Some(_client_auth_key),
             Some(_challenge_response)) =>
            {
                let mut success = true;
                success &= self.client_allowed;
                success &= self.client_requested_endpoint_valid;
                success &= self.client_proof_signature_valid;
                success &= self.client_auth_signature_valid;
                success &= self.challenge_response_valid;

                self.state = IdentityServerState::ChallengeVerificationResponseSent;
                if success {
                    let endpoint_private_key = Ed25519PrivateKey::generate();
                    let endpoint_service_id = V3OnionServiceId::from_private_key(&endpoint_private_key);
                    self.endpoint_private_key = Some(endpoint_private_key);
                    Some((
                        send_response_request_cookie,
                        Some(Bson::String(endpoint_service_id.to_string())),
                        ErrorCode::Success))
                } else {
                    Some((
                        send_response_request_cookie,
                        None,
                        ErrorCode::Runtime(GoslingError::Failure as i32)))
                }
            },
            _ => None,
        }
    }
}

enum EndpointClientEvent {
    HandshakeCompleted
}

#[derive(Debug, PartialEq)]
enum EndpointClientState {
    BeginHandshake,
    WaitingForServerCookie,
    WaitingForProofVerification,
    HandshakeComplete,
}

struct EndpointClient<RW> {
    // session data
    rpc: Session<RW,RW>,
    server_service_id: V3OnionServiceId,
    requested_channel: String,
    client_ed25519_private: Ed25519PrivateKey,

    // state machine data
    state: EndpointClientState,
    begin_handshake_request_cookie: Option<RequestCookie>,
    send_response_request_cookie: Option<RequestCookie>,
}

impl<RW> EndpointClient<RW> where RW : std::io::Read + std::io::Write + Send {
    fn new(
        rpc: Session<RW,RW>,
        server_service_id: V3OnionServiceId,
        requested_channel: String,
        client_ed25519_private: Ed25519PrivateKey) -> Self {
        Self{
            rpc,
            server_service_id,
            requested_channel,
            client_ed25519_private,
            state: EndpointClientState::BeginHandshake,
            begin_handshake_request_cookie: None,
            send_response_request_cookie: None,
        }
    }

    fn update(&mut self) -> Result<Option<EndpointClientEvent>> {

        ensure!(self.state != EndpointClientState::HandshakeComplete);

        // update our rpc session
        self.rpc.update(None)?;

        // client state machine
        match (
            &self.state,
            self.begin_handshake_request_cookie,
            self.send_response_request_cookie) {
            (&EndpointClientState::BeginHandshake, None, None) => {
                self.begin_handshake_request_cookie = Some(self.rpc.client_call(
                    "gosling_endpoint",
                    "begin_handshake",
                    0,
                    doc!{
                        "version" : bson::Bson::String(GOSLING_VERSION.to_string()),
                        "channel" : bson::Bson::String(self.requested_channel.clone()),
                    })?);
                self.state = EndpointClientState::WaitingForServerCookie;
                Ok(None)
            },
            (&EndpointClientState::WaitingForServerCookie, Some(begin_handshake_request_cookie), None) => {
                if let Some(response) = self.rpc.client_next_response() {
                    let result = match response {
                        Response::Pending{cookie} => {
                            ensure!(cookie == begin_handshake_request_cookie, "received unexpected pending response");
                            return Ok(None);
                        },
                        Response::Error{cookie, error_code} => {
                            ensure!(cookie == begin_handshake_request_cookie, "received unexpected error response; rpc error_code: {}", error_code);
                            bail!("rpc error_code: {}", error_code);
                        },
                        Response::Success{cookie, result} => {
                            ensure!(cookie == begin_handshake_request_cookie, "received unexpected success response");
                            result
                        },
                    };

                    if let bson::Bson::Document(result) = result {
                        if let Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: server_cookie})) = result.get("server_cookie") {
                            // build arguments for send_response()

                            // client_cookie
                            let mut client_cookie: ClientCookie = Default::default();
                            OsRng.fill_bytes(&mut client_cookie);

                            // client_identity
                            let client_ed25519_public = Ed25519PublicKey::from_private_key(&self.client_ed25519_private);
                            let client_service_id = V3OnionServiceId::from_public_key(&client_ed25519_public);
                            let client_identity = client_service_id.to_string();

                            // client_identity_proof_signature
                            let server_cookie: ServerCookie = match server_cookie.clone().try_into() {
                                Ok(server_cookie) => server_cookie,
                                Err(_) => bail!("invalid server cookie length"),
                            };
                            let client_identity_proof = build_client_proof(
                                DomainSeparator::GoslingEndpoint,
                                &self.requested_channel,
                                &client_service_id,
                                &self.server_service_id,
                                &client_cookie,
                                &server_cookie,
                            )?;
                            let client_identity_proof_signature = self.client_ed25519_private.sign_message(&client_identity_proof);

                            // build our args object for rpc call
                            let args = doc!{
                                "client_cookie" : Bson::Binary(bson::Binary{subtype: BinarySubtype::Generic, bytes: client_cookie.to_vec()}),
                                "client_identity" : Bson::String(client_identity),
                                "client_identity_proof_signature" : Bson::Binary(bson::Binary{subtype: BinarySubtype::Generic, bytes: client_identity_proof_signature.to_bytes().to_vec()}),
                            };

                            // make rpc call
                            self.send_response_request_cookie = Some(self.rpc.client_call(
                                "gosling_endpoint",
                                "send_response",
                                0,
                                args)?);

                            self.state = EndpointClientState::WaitingForProofVerification;
                        }
                    } else {
                        bail!("begin_handshake() returned unexpected value: {}", result);
                    }
                }
                Ok(None)
            },
            (&EndpointClientState::WaitingForProofVerification, Some(_begin_handshake_request_cookie), Some(send_response_request_cookie)) => {
                if let Some(response) = self.rpc.client_next_response() {
                    let result = match response {
                        Response::Pending{cookie} => {
                            ensure!(cookie == send_response_request_cookie, "received unexpected pending response");
                            return Ok(None);
                        },
                        Response::Error{cookie, error_code} => {
                            ensure!(cookie == send_response_request_cookie, "received unexpected error response; rpc error_code: {}", error_code);
                            bail!("rpc error_code: {}", error_code);
                        },
                        Response::Success{cookie, result} => {
                            ensure!(cookie == send_response_request_cookie, "received unexpected success response");
                            result
                        },
                    };

                    if let Bson::Document(result) = result {
                        ensure!(result.is_empty());

                        self.state = EndpointClientState::HandshakeComplete;
                        return Ok(Some(EndpointClientEvent::HandshakeCompleted));
                    }
                }
                Ok(None)
            },
            _ => {
                bail!("unexpected state: state: {:?}, begin_handshake_request_cookie: {:?}", self.state, self.begin_handshake_request_cookie);
            },
        }

    }
}

//
// Endpoint Server
//

enum EndpointServerEvent {
    ChannelRequestReceived{
        requested_channel: String
    },
    // endpoint server has acepted incoming channel request from identity client
    HandshakeCompleted{
        client_service_id: V3OnionServiceId,
        channel_name: String,
    },
    // endpoint server has reject an incoming channel request
    HandshakeRejected{
        client_allowed: bool,
        client_requested_channel_valid: bool,
        client_proof_signature_valid: bool,
    },
}

#[derive(Debug, PartialEq)]
enum EndpointServerState {
    WaitingForBeginHandshake,
    WaitingForSendResponse,
    HandledSendResponse,
    HandshakeComplete,
}

struct EndpointServer<RW> {
    // Session Data
    rpc: Option<Session<RW,RW>>,
    server_cookie: ServerCookie,
    client_identity: V3OnionServiceId,
    server_identity: V3OnionServiceId,

    // State Machine Data
    state: EndpointServerState,
    requested_channel: Option<String>,
    handshake_succeeded: Option<bool>
}

impl<RW> EndpointServer<RW> where RW : std::io::Read + std::io::Write + Send {
    pub fn new(
        rpc: Session<RW,RW>,
        client_identity: V3OnionServiceId,
        server_identity: V3OnionServiceId) -> Self {

        // generate server cookie
        let mut server_cookie: ServerCookie = Default::default();
        OsRng.fill_bytes(&mut server_cookie);


        EndpointServer{
            rpc: Some(rpc),
            server_cookie,
            client_identity,
            server_identity,
            state: EndpointServerState::WaitingForBeginHandshake,
            requested_channel: None,
            handshake_succeeded: None,
        }
    }

    pub fn update(&mut self) -> Result<Option<EndpointServerEvent>> {
        if let Some(mut rpc) = std::mem::take(&mut self.rpc) {
            rpc.update(Some(&mut [self]))?;
            self.rpc = Some(rpc);
        }

        match(&self.state,
              self.requested_channel.as_ref(),
              self.handshake_succeeded) {
            (&EndpointServerState::WaitingForBeginHandshake,
             None, // requesed channel
             None) // handshake_succeeded
            => {},
            (&EndpointServerState::WaitingForSendResponse,
             Some(_requested_channel),
             None) // handshake_succeeded
            => {},
            (&EndpointServerState::HandledSendResponse,
             Some(requested_channel),
             Some(handshake_succeeded))
            => {
                self.state = EndpointServerState::HandshakeComplete;
                if handshake_succeeded {
                    return Ok(Some(EndpointServerEvent::HandshakeCompleted{
                        client_service_id: self.client_identity.clone(),
                        channel_name: requested_channel.clone(),
                    }));
                }
            },
            _ => {
                bail!("unexpected state: state: {:?}, requested_channel: {:?}, handshake_succeeded: {:?}", self.state, self.requested_channel, self.handshake_succeeded);
            }
        }

        Ok(None)
    }

    fn handle_begin_handshake(
        &mut self,
        version: String,
        channel: String) -> Result<bson::Bson, GoslingError> {

        if version != GOSLING_VERSION {
            return Err(GoslingError::BadVersion);
        }

        // save off requested channel
        self.requested_channel = Some(channel);

        // return result
        let retval = doc!{"server_cookie" : Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: self.server_cookie.to_vec()})};
        self.state = EndpointServerState::WaitingForSendResponse;

        Ok(Bson::Document(retval))
    }

    fn handle_send_response(
        &mut self,
        client_cookie: ClientCookie,
        client_identity: V3OnionServiceId,
        client_identity_proof_signature: Ed25519Signature) -> Result<bson::Bson, GoslingError> {

        // is client on the allow list
        let client_allowed = client_identity == self.client_identity;

        // convert client_identity to client's public ed25519 key
        if let (Ok(client_identity_key), Some(requested_channel)) = (Ed25519PublicKey::from_service_id(&client_identity), self.requested_channel.as_ref()) {
            // construct + verify client proof
            if let Ok(client_proof) = build_client_proof(
                                            DomainSeparator::GoslingEndpoint,
                                            requested_channel,
                                            &client_identity,
                                            &self.server_identity,
                                            &client_cookie,
                                            &self.server_cookie) {
                let client_proof_signature_valid = client_identity_proof_signature.verify(&client_proof, &client_identity_key);

                if client_allowed && client_proof_signature_valid {
                    // return empty doc
                    self.handshake_succeeded = Some(true);
                    self.state = EndpointServerState::HandledSendResponse;
                    return Ok(Bson::Document(doc!{}));
                }
            };
        }

        self.handshake_succeeded = Some(false);
        self.state = EndpointServerState::HandledSendResponse;
        Err(GoslingError::Failure)
    }
}

impl<RW> ApiSet for EndpointServer<RW> where RW : std::io::Read + std::io::Write + Send {
    fn namespace(&self) -> &str {
        "gosling_endpoint"
    }

    fn exec_function(
        &mut self,
        name: &str,
        version: i32,
        mut args: bson::document::Document,
        request_cookie: Option<RequestCookie>) -> Result<Option<bson::Bson>, ErrorCode> {

        let request_cookie = match request_cookie {
            Some(request_cookie) => request_cookie,
            None => return Err(ErrorCode::Runtime(GoslingError::RequestCookieRequired as i32)),
        };

        match
            (name, version,
             &self.state,
             self.requested_channel.as_ref()) {
            ("begin_handshake", 0,
            &EndpointServerState::WaitingForBeginHandshake,
            None) // requested_channel
            => {
                if let (Some(Bson::String(version)),
                        Some(Bson::String(channel_name))) =
                       (args.remove("version"),
                        args.remove("channel")) {
                    match self.handle_begin_handshake(version, channel_name) {
                        Ok(result) => Ok(Some(result)),
                        Err(err) => Err(ErrorCode::Runtime(err as i32)),
                    }
                } else {
                    Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32))
                }
            },
            ("send_response", 0,
            &EndpointServerState::WaitingForSendResponse,
            Some(_requested_channel))
            => {
                if let (Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: client_cookie})),
                        Some(Bson::String(client_identity)),
                        Some(Bson::Binary(Binary{subtype: BinarySubtype::Generic, bytes: client_identity_proof_signature}))) =
                       (args.remove("client_cookie"),
                        args.remove("client_identity"),
                        args.remove("client_identity_proof_signature")) {
                    // client_cookie
                    let client_cookie : ClientCookie = match client_cookie.try_into() {
                        Ok(client_cookie) => client_cookie,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    // client_identiity
                    let client_identity = match V3OnionServiceId::from_string(&client_identity) {
                        Ok(client_identity) => client_identity,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    // client_identity_proof_signature
                    let client_identity_proof_signature : [u8; ED25519_SIGNATURE_SIZE] = match client_identity_proof_signature.try_into() {
                        Ok(client_identity_proof_signature) => client_identity_proof_signature,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    let client_identity_proof_signature = match Ed25519Signature::from_raw(&client_identity_proof_signature) {
                        Ok(client_identity_proof_signature) => client_identity_proof_signature,
                        Err(_) => return Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32)),
                    };

                    match self.handle_send_response(client_cookie, client_identity, client_identity_proof_signature) {
                        Ok(result) => Ok(Some(result)),
                        Err(err) => Err(ErrorCode::Runtime(err as i32)),
                    }
                } else {
                    Err(ErrorCode::Runtime(GoslingError::InvalidArg as i32))
                }
            },
            _ => Ok(None),
        }
    }

    fn next_result(&mut self) -> Option<(RequestCookie, Option<bson::Bson>, ErrorCode)> {
        None
    }
}

/// cbindgen:ignore
pub type HandshakeHandle = usize;
//
// The root Gosling Context object
//
pub struct Context {
    // our tor instance
    tor_manager : TorManager,
    bootstrap_complete: bool,
    identity_port : u16,
    endpoint_port : u16,

    //
    // Servers and Clients for in-process handshakes
    //
    next_handshake_handle: HandshakeHandle,
    identity_clients: BTreeMap<HandshakeHandle, IdentityClient<OnionStream>>,
    identity_servers: BTreeMap<HandshakeHandle, IdentityServer<OnionStream>>,
    endpoint_clients : BTreeMap<HandshakeHandle, (EndpointClient<OnionStream>, TcpStream)>,
    endpoint_servers : BTreeMap<HandshakeHandle, (EndpointServer<OnionStream>, TcpStream)>,

    //
    // Listeners for incoming connections
    //
    identity_listener : Option<OnionListener>,
    // maps the endpoint service id to the enpdoint name, alowed client, onion listener tuple
    endpoint_listeners : HashMap<V3OnionServiceId, (String, V3OnionServiceId, OnionListener)>,

    //
    // Server Config Data
    //

    // Private key behind the identity onion service
    identity_private_key : Ed25519PrivateKey,
    // Identity server's service id
    identity_service_id : V3OnionServiceId,
}

pub enum ContextEvent {

    //
    // Tor Events
    //

    // bootstrap progress
    TorBootstrapStatusReceived{
        progress: u32,
        tag: String,
        summary: String
    },

    // bootstrapping finished
    TorBootstrapCompleted,

    // tor log
    TorLogReceived{
        line: String
    },

    //
    // Identity Client Events
    //

    // identity client has received a challenge request from an identy server
    // to continue the handshake, call Context::identity_client_handle_challenge_received
    IdentityClientChallengeReceived{
        handle: HandshakeHandle,
        identity_service_id: V3OnionServiceId,
        endpoint_name: String,
        endpoint_challenge: bson::document::Document,
    },

    // identity client successfully completes identity handshake
    IdentityClientHandshakeCompleted{
        handle: HandshakeHandle,
        identity_service_id: V3OnionServiceId,
        endpoint_service_id: V3OnionServiceId,
        endpoint_name: String,
        client_auth_private_key: X25519PrivateKey
    },

    // identity client handshake failed
    IdentityClientHandshakeFailed{
        handle: HandshakeHandle,
        reason: Option<error::Error>,
    },

    // identity server onion service published
    IdentityServerPublished,

    // identity server has received incoming connection
    IdentityServerHandshakeStarted{
        handle: HandshakeHandle
    },

    // identity server receives request from identity client
    // to continue the handshake, call Context::identity_server_handle_endpoint_request_received()
    IdentityServerEndpointRequestReceived{
        handle: HandshakeHandle,
        client_service_id: V3OnionServiceId,
        requested_endpoint: String,
    },

    // identity server receives challenge response from identity client
    // to continue the handshake, call Context::identity_server_handle_challenge_response_received
    IdentityServerChallengeResponseReceived{
        handle: HandshakeHandle,
        challenge_response: bson::document::Document,
    },

    // identity server supplies a new endpoint server to an identity client
    IdentityServerHandshakeCompleted{
        handle: HandshakeHandle,
        endpoint_private_key: Ed25519PrivateKey,
        endpoint_name: String,
        client_service_id: V3OnionServiceId,
        client_auth_public_key: X25519PublicKey
    },

    // identity server handshake explicitly rejected client handshake
    IdentityServerHandshakeRejected{
        handle: HandshakeHandle,
        client_allowed: bool,
        client_requested_endpoint_valid: bool,
        client_proof_signature_valid: bool,
        client_auth_signature_valid: bool,
        challenge_response_valid: bool,
    },

    // identity server handshake failed due to error
    IdentityServerHandshakeFailed{
        handle: HandshakeHandle,
        reason: Option<error::Error>,
    },

    //
    // Endpoint Client Events
    //

    // endpoint client successfully opens a channel on an endpoint server
    EndpointClientHandshakeCompleted{
        // handle: HandshakeHandle,
        endpoint_service_id: V3OnionServiceId,
        channel_name: String,
        stream: TcpStream
    },


    // identity client handshake aborted
    EndointClientHandshakeFailed{
        handle: HandshakeHandle,
        reason: Option<error::Error>,
    },

    //
    // Endpint Server Events
    //

    // endpoint server onion service published
    EndpointServerPublished{
        endpoint_service_id: V3OnionServiceId,
        endpoint_name: String,
    },

    EndpointServerHandshakeStarted{
        handle: HandshakeHandle,
    },

    // endpoint server receives request from endpoint client
    // to continue the handshake, call Context::endpoint_server_handle_channel_request_received()
    EndpointServerChannelRequestReceived{
        handle: HandshakeHandle,
        // client_service_id: V3OnionServiceId,
        endpoint_service_id: V3OnionServiceId,
        requested_channel: String,
    },

    // endpoint server has acepted incoming channel request from identity client
    EndpointServerHandshakeCompleted{
        handle: HandshakeHandle,
        endpoint_service_id: V3OnionServiceId,
        client_service_id: V3OnionServiceId,
        channel_name:  String,
        stream: TcpStream
    },

    // endpoint server handshake explicitly rejected client handshake
    EndpointServerHandshakeRejected{
        handle: HandshakeHandle,
        client_allowed: bool,
        client_requested_channel_valid: bool,
        client_proof_signature_valid: bool,
    },

    // endpoint server request failed
    EndpointServerRequestFailed{
        handle: HandshakeHandle,
        reason: Option<error::Error>,
    },

}

impl Context {
    pub fn new(
        tor_working_directory: &Path,
        identity_port: u16,
        endpoint_port: u16,
        identity_private_key: Ed25519PrivateKey) -> Result<Self> {

        let tor_manager = TorManager::new(tor_working_directory)?;

        let identity_service_id = V3OnionServiceId::from_private_key(&identity_private_key);

        Ok(Self {
            tor_manager,
            bootstrap_complete: false,
            identity_port,
            endpoint_port,

            next_handshake_handle: Default::default(),
            identity_clients: Default::default(),
            identity_servers: Default::default(),
            endpoint_clients: Default::default(),
            endpoint_servers: Default::default(),

            identity_listener: None,
            endpoint_listeners: Default::default(),

            identity_private_key,
            identity_service_id,
        })
    }

    pub fn bootstrap(&mut self) -> Result<()> {
        self.tor_manager.bootstrap()
    }

    pub fn identity_client_begin_handshake(
        &mut self,
        identity_server_id: V3OnionServiceId,
        endpoint: &str) -> Result<HandshakeHandle> {
        ensure!(self.bootstrap_complete);
        // open tcp stream to remove ident server
        let stream = self.tor_manager.connect(&identity_server_id, self.identity_port, None)?;
        resolve!(stream.set_nonblocking(true));
        let client_rpc = Session::new(stream.try_clone()?, stream);

        let ident_client = IdentityClient::new(
            client_rpc,
            identity_server_id,
            endpoint.to_string(),
            self.identity_private_key.clone(),
            X25519PrivateKey::generate());

        let handshake_handle = self.next_handshake_handle;
        self.next_handshake_handle += 1;
        self.identity_clients.insert(handshake_handle, ident_client);

        Ok(handshake_handle)
    }

    pub fn identity_client_abort_identity_handshake(
        &mut self,
        handle: HandshakeHandle) -> Result<()> {

        if let Some(_identity_client) = self.identity_clients.remove(&handle) {
            Ok(())
        } else {
            bail!("identity client with handle {} not found", handle);
        }
    }

    // sends an endpoint challenge response to a connected identity server as
    // part of an identity handshake session
    pub fn identity_client_handle_challenge_received(
        &mut self,
        handle: HandshakeHandle,
        challenge_response: bson::document::Document) -> Result<()> {

        if let Some(identity_client) = self.identity_clients.get_mut(&handle) {
            identity_client.send_response(challenge_response)?;
            Ok(())
        } else {
            bail!("no handshake associaed with handle '{}'", handle);
        }
    }

    pub fn identity_server_start(&mut self) -> Result<()> {
        ensure!(self.bootstrap_complete);
        ensure!(self.identity_listener.is_none());

        let identity_listener = self.tor_manager.listener(&self.identity_private_key, self.identity_port, None)?;
        identity_listener.set_nonblocking(true)?;

        self.identity_listener = Some(identity_listener);
        Ok(())
    }

    pub fn identity_server_stop(&mut self) -> Result<()> {
        ensure!(self.bootstrap_complete);
        // clear out current identduciton listener
        self.identity_listener = None;
        // clear out any in-process identity handshakes
        self.identity_servers = Default::default();
        Ok(())
    }

    // sends an endpoint challenge to a connected identity client as part of
    // an identity handshake session abd save off wheether the requested endpoint
    // is supported
    pub fn identity_server_handle_endpoint_request_received(
        &mut self,
        handle: HandshakeHandle,
        client_allowed: bool,
        endpoint_supported: bool,
        endpoint_challenge: bson::document::Document) -> Result<()> {

        if let Some(identity_server) = self.identity_servers.get_mut(&handle) {
            identity_server.send_challenge(client_allowed, endpoint_supported, endpoint_challenge)
        } else {
            bail!("no handshake associated with handle '{}'", handle);
        }

    }

    // confirm that a received endpoint challenge response is valid
    pub fn identity_server_handle_challenge_response_received(
        &mut self,
        handle: HandshakeHandle,
        challenge_response_valid: bool) -> Result<()> {

        if let Some(identity_server) = self.identity_servers.get_mut(&handle) {
            identity_server.send_challenge_verification(challenge_response_valid)
        } else {
            bail!("no handshake associated with handle '{}'", handle);
        }
    }

    pub fn endpoint_client_begin_handshake(
        &mut self,
        endpoint_server_id: V3OnionServiceId,
        client_auth_key: X25519PrivateKey,
        channel: String) -> Result<()> {
        ensure!(self.bootstrap_complete);
        self.tor_manager.add_client_auth(&endpoint_server_id, &client_auth_key)?;
        let stream = self.tor_manager.connect(&endpoint_server_id, self.endpoint_port, None)?;
        resolve!(stream.set_nonblocking(true));
        let client_rpc = Session::new(stream.try_clone()?, stream.try_clone()?);

        let endpoint_client = EndpointClient::new(
            client_rpc,
            endpoint_server_id,
            channel,
            self.identity_private_key.clone());
               let handshake_handle = self.next_handshake_handle;

        self.next_handshake_handle += 1;
        self.endpoint_clients.insert(handshake_handle, (endpoint_client, stream.into()));
        Ok(())
    }

    pub fn endpoint_client_abort_handshake(
        &mut self,
        handle: HandshakeHandle) -> Result<()> {

        if let Some(_endpoint_client) = self.endpoint_clients.remove(&handle) {
            Ok(())
        } else {
            bail!("endpoint client with handle {} not found", handle);
        }
    }

    pub fn endpoint_server_start(
        &mut self,
        endpoint_private_key: Ed25519PrivateKey,
        endpoint_name: String,
        client_identity: V3OnionServiceId,
        client_auth: X25519PublicKey) -> Result<()> {
        ensure!(self.bootstrap_complete);
        let endpoint_listener = self.tor_manager.listener(&endpoint_private_key, self.endpoint_port, Some(&[client_auth]))?;
        endpoint_listener.set_nonblocking(true)?;

        let endpoint_public_key = Ed25519PublicKey::from_private_key(&endpoint_private_key);
        let endpoint_service_id = V3OnionServiceId::from_public_key(&endpoint_public_key);

        self.endpoint_listeners.insert(endpoint_service_id, (endpoint_name, client_identity, endpoint_listener));
        Ok(())
    }

    pub fn endpoint_server_handle_channel_request_received(
        &mut self,
        handle: HandshakeHandle,
        channel_supported: bool) -> Result<()> {
        // TODO
        bail!("not implemented");
    }

    pub fn endpoint_server_stop(
        &mut self,
        endpoint_identity: V3OnionServiceId) -> Result<()> {
        ensure!(self.bootstrap_complete);

        if let Some(_listener) = self.endpoint_listeners.remove(&endpoint_identity) {
            Ok(())
        } else {
            bail!("endpoint server with service id {} not found", endpoint_identity.to_string());
        }
    }

    pub fn update(&mut self) -> Result<Vec<ContextEvent>> {

        // first handle new identity connections
        if let Some(listener) = &mut self.identity_listener {
            if let Some(stream) = listener.accept()? {
                resolve!(stream.set_nonblocking(true));
                let identity_public_key = Ed25519PublicKey::from_private_key(&self.identity_private_key);
                let server_service_id = V3OnionServiceId::from_public_key(&identity_public_key);
                let server_rpc = Session::new(stream.try_clone()?, stream.try_clone()?);
                let ident_server = IdentityServer::new(
                    server_rpc,
                    server_service_id);

                let handshake_handle = self.next_handshake_handle;
                self.next_handshake_handle += 1;
                self.identity_servers.insert(handshake_handle, ident_server);
            }
        }

        // next handle new endpoint connections
        for (endpoint_service_id, (_endpoint_name, allowed_client, listener)) in self.endpoint_listeners.iter_mut() {
            if let Some(stream) = listener.accept()? {
                resolve!(stream.set_nonblocking(true));
                let server_rpc = Session::new(stream.try_clone()?, stream.try_clone()?);
                let endpoint_server = EndpointServer::new(
                    server_rpc,
                    allowed_client.clone(),
                    endpoint_service_id.clone());

                let handshake_handle = self.next_handshake_handle;
                self.next_handshake_handle += 1;
                self.endpoint_servers.insert(handshake_handle, (endpoint_server, stream.into()));
            }
        }

        // events to return
        let mut events : Vec<ContextEvent> = Default::default();

        // consume tor events
        for event in self.tor_manager.update()?.drain(..) {
            match event {
                Event::BootstrapStatus{progress,tag,summary} => {
                    events.push(ContextEvent::TorBootstrapStatusReceived{progress, tag, summary});
                },
                Event::BootstrapComplete => {
                    events.push(ContextEvent::TorBootstrapCompleted);
                    self.bootstrap_complete = true;
                },
                Event::LogReceived{line} => {
                    events.push(ContextEvent::TorLogReceived{line});
                },
                Event::OnionServicePublished{service_id} => {
                    if service_id == self.identity_service_id {
                        events.push(ContextEvent::IdentityServerPublished);
                    } else if let Some((endpoint_name, _, _)) = self.endpoint_listeners.get(&service_id) {
                        events.push(ContextEvent::EndpointServerPublished{
                            endpoint_service_id: service_id,
                            endpoint_name: endpoint_name.clone(),
                        });
                    }
                },
            }
        }

        // update the ident client handshakes
        // TODO: switch to drain_filter once it comes out of nightly
        {
            let handles : Vec<HandshakeHandle> = self.identity_clients.keys().cloned().collect();
            for handle in handles {
                let identity_client = self.identity_clients.get_mut(&handle).unwrap();
                let remove = match identity_client.update() {
                    Ok(Some(IdentityClientEvent::ChallengeReceived{
                        identity_service_id,
                        endpoint_name,
                        endpoint_challenge,
                    })) => {
                        events.push(
                            ContextEvent::IdentityClientChallengeReceived{
                                handle,
                                identity_service_id,
                                endpoint_name,
                                endpoint_challenge});
                        false
                    },
                    Ok(Some(IdentityClientEvent::HandshakeCompleted{
                        identity_service_id,
                        endpoint_service_id,
                        endpoint_name,
                        client_auth_private_key,
                    })) => {
                        events.push(
                            ContextEvent::IdentityClientHandshakeCompleted{
                                handle,
                                identity_service_id,
                                endpoint_service_id,
                                endpoint_name,
                                client_auth_private_key});
                        true
                    },
                    Ok(None) => false,
                    Err(err) => {
                        events.push(
                            ContextEvent::IdentityClientHandshakeFailed{
                                handle,
                                reason: Some(err),
                            });
                        true
                    },
                };
                if remove {
                    self.identity_clients.remove(&handle);
                }
            }
        }

        // update the ident server handshakes
        {
            // TODO: switch to drain_filter once it comes out of nightly
            let handles : Vec<HandshakeHandle> = self.identity_servers.keys().cloned().collect();
            for handle in handles {
                let identity_server = self.identity_servers.get_mut(&handle).unwrap();
                let remove = match identity_server.update() {
                    Ok(Some(IdentityServerEvent::EndpointRequestReceived{client_service_id, requested_endpoint})) => {
                        events.push(
                            ContextEvent::IdentityServerEndpointRequestReceived{
                                handle,
                                client_service_id,
                                requested_endpoint});
                        false
                    },
                    Ok(Some(IdentityServerEvent::ChallengeResponseReceived{
                        challenge_response})) => {
                        events.push(
                            ContextEvent::IdentityServerChallengeResponseReceived{
                                handle,
                                challenge_response});
                        false
                    },
                    Ok(Some(IdentityServerEvent::HandshakeCompleted{
                        endpoint_private_key,
                        endpoint_name,
                        client_service_id,
                        client_auth_public_key,
                    })) => {
                        events.push(
                            ContextEvent::IdentityServerHandshakeCompleted{
                                handle,
                                endpoint_private_key,
                                endpoint_name,
                                client_service_id,
                                client_auth_public_key});
                        true
                    },
                    Ok(Some(IdentityServerEvent::HandshakeRejected{
                        client_allowed,
                        client_requested_endpoint_valid,
                        client_proof_signature_valid,
                        client_auth_signature_valid,
                        challenge_response_valid,
                    })) => {
                        println!("failure!");
                        println!(" client_allowed: {}", client_allowed);
                        println!(" client_requested_endpoint_valid: {}", client_requested_endpoint_valid);
                        println!(" client_proof_signature_valid: {}", client_proof_signature_valid);
                        println!(" client_auth_signature_valid: {}", client_auth_signature_valid);
                        println!(" challenge_response_valid: {}", challenge_response_valid);
                        true
                    },
                    Ok(None) => false,
                    Err(err) => {
                        events.push(
                            ContextEvent::IdentityServerHandshakeFailed{
                                handle,
                                reason: Some(err),
                            });
                        true
                    },
                };

                if remove {
                    self.identity_servers.remove(&handle);
                }
            }
        }

        // update the endpoint client handshakes
        // TODO: switch to drain_filter once it comes out of nightly
        {
            let handles : Vec<HandshakeHandle> = self.endpoint_clients.keys().cloned().collect();
            for handle in handles {
                let (endpoint_client, stream) = self.endpoint_clients.get_mut(&handle).unwrap();
                let remove = match endpoint_client.update() {
                    Ok(Some(EndpointClientEvent::HandshakeCompleted)) => {
                        events.push(
                            ContextEvent::EndpointClientHandshakeCompleted{
                                endpoint_service_id: endpoint_client.server_service_id.clone(),
                                channel_name: endpoint_client.requested_channel.clone(),
                                stream: resolve!(stream.try_clone()),
                            });
                        true
                    },
                    Ok(None) => false,
                    Err(err) => {
                        println!("error: {:?}", err);
                        true
                    },
                };

                if remove {
                    self.endpoint_clients.remove(&handle);
                }
            }
        }

        // update the endpoint server handshakes
        // TODO: switch to drain_filter once it comes out of nightly
        {
            let handles : Vec<HandshakeHandle> = self.endpoint_servers.keys().cloned().collect();
            for handle in handles {
                let (endpoint_server, stream) = self.endpoint_servers.get_mut(&handle).unwrap();
                let remove = match endpoint_server.update() {
                    Ok(Some(EndpointServerEvent::ChannelRequestReceived{
                        requested_channel
                    })) => {
                        events.push(
                            ContextEvent::EndpointServerChannelRequestReceived{
                                handle,
                                endpoint_service_id: endpoint_server.server_identity.clone(),
                                requested_channel
                            });
                        false
                    },
                    Ok(Some(EndpointServerEvent::HandshakeCompleted{
                        client_service_id,
                        channel_name})) => {

                        if let Ok(stream) = stream.try_clone() {
                            events.push(
                                ContextEvent::EndpointServerHandshakeCompleted{
                                    handle,
                                    endpoint_service_id: endpoint_server.server_identity.clone(),
                                    client_service_id,
                                    channel_name,
                                    stream});
                        }
                        true
                    },
                    Ok(Some(EndpointServerEvent::HandshakeRejected{
                        client_allowed,
                        client_requested_channel_valid,
                        client_proof_signature_valid})) => {
                        events.push(
                            ContextEvent::EndpointServerHandshakeRejected{
                                handle,
                                client_allowed,
                                client_requested_channel_valid,
                                client_proof_signature_valid});
                        true
                    },
                    Ok(None) => false,
                    Err(_) => true,
                };

                if remove {
                    self.endpoint_servers.remove(&handle);
                }
            }
        }

        Ok(events)
    }
}


//
// Tests
//

#[cfg(test)]
fn identity_test(
    client_blocked: bool,
    client_requested_endpoint: &str,
    client_requested_endpoint_valid: bool,
    server_challenge: bson::document::Document,
    client_response: bson::document::Document,
    server_expected_response: bson::document::Document,
    should_fail: bool) -> Result<()> {
    // test sockets
    let stream1 = MemoryStream::new();
    let stream2 = MemoryStream::new();

    // client setup
    let client_ed25519_private = Ed25519PrivateKey::generate();

    // server setup
    let server_ed25519_private = Ed25519PrivateKey::generate();
    let server_ed25519_public = Ed25519PublicKey::from_private_key(&server_ed25519_private);
    let server_service_id = V3OnionServiceId::from_public_key(&server_ed25519_public);

    // rpc setup
    let client_rpc = Session::new(stream1.clone(), stream2.clone());
    let mut ident_client = IdentityClient::new(
        client_rpc,
        server_service_id.clone(),
        client_requested_endpoint.to_string(),
        client_ed25519_private,
        X25519PrivateKey::generate());

    let server_rpc = Session::new(stream2, stream1);
    let mut ident_server = IdentityServer::new(
        server_rpc,
        server_service_id.clone());

    let mut failure_ocurred = false;
    let mut server_complete = false;
    let mut client_complete = false;
    while !server_complete && !client_complete {
        if !server_complete {
            match ident_server.update() {
                Ok(Some(IdentityServerEvent::EndpointRequestReceived{client_service_id, requested_endpoint})) => {
                    println!("server challenge send: client_service_id {}, requested_endpoint: {}", client_service_id.to_string(), requested_endpoint);
                    let client_allowed = !client_blocked;
                    ident_server.send_challenge(client_allowed, client_requested_endpoint_valid, server_challenge.clone())?;
                },
                Ok(Some(IdentityServerEvent::ChallengeResponseReceived{challenge_response})) => {
                    println!("server challenge repsonse received");
                    ident_server.send_challenge_verification(challenge_response == server_expected_response)?;
                },
                Ok(Some(IdentityServerEvent::HandshakeCompleted{endpoint_private_key: _, endpoint_name,client_service_id,client_auth_public_key: _})) => {
                    ensure!(endpoint_name == client_requested_endpoint);
                    println!("server complete! client_service_id : {}", client_service_id.to_string());
                    server_complete = true;
                },
                Ok(Some(IdentityServerEvent::HandshakeRejected{client_allowed, client_requested_endpoint_valid, client_proof_signature_valid, client_auth_signature_valid, challenge_response_valid})) => {
                    println!("server complete! client request rejected");
                    println!(" client_allowed: {}", client_allowed);
                    println!(" client_requested_endpoint_valid: {}", client_requested_endpoint_valid);
                    println!(" client_proof_signature_valid: {}", client_proof_signature_valid);
                    println!(" client_auth_signature_valid: {}", client_auth_signature_valid);
                    println!(" client_response_valid: {}", challenge_response_valid);
                    server_complete = true;
                    failure_ocurred = true;
                },
                Ok(None) => {},
                Err(err) => {
                    println!("server failure: {:?}", err);
                    server_complete = true;
                    failure_ocurred = true;
                },
            }
        }

        if !client_complete {
            match ident_client.update() {
                Ok(Some(IdentityClientEvent::ChallengeReceived{identity_service_id, endpoint_name, endpoint_challenge})) => {
                    ensure!(identity_service_id == server_service_id);
                    println!("client challenge request received: identity_service_id: {}, endpoint_name: {}, endpoint_challenge: {}", identity_service_id.to_string(), endpoint_name, endpoint_challenge);
                    ident_client.send_response(client_response.clone())?;
                },
                Ok(Some(IdentityClientEvent::HandshakeCompleted{identity_service_id,endpoint_service_id,endpoint_name,client_auth_private_key: _})) => {
                    ensure!(identity_service_id == server_service_id);
                    ensure!(endpoint_name == client_requested_endpoint);
                    println!("client complete! endpoint_server : {}", endpoint_service_id.to_string());
                    client_complete = true;
                },
                Ok(None) => {},
                Err(err) => {
                    println!("client failure: {:?}", err);
                    client_complete = true;
                    failure_ocurred = true;
                },
            }
        }
    }

    ensure!(failure_ocurred == should_fail);
    Ok(())
}

#[test]
fn test_identity_handshake() -> Result<()> {

    println!("Sucessful ---");
    {
        let client_blocked: bool = false;
        let client_requested_endpoint: &str = "endpoint";
        let client_requested_endpoint_valid: bool = true;
        let server_challenge: bson::document::Document = doc!("msg": "Speak friend and enter");
        let client_response: bson::document::Document = doc!("msg": "Mellon");
        let server_expected_response: bson::document::Document = doc!("msg": "Mellon");
        let should_fail: bool = false;
        identity_test(
            client_blocked,
            client_requested_endpoint,
            client_requested_endpoint_valid,
            server_challenge,
            client_response,
            server_expected_response,
            should_fail)?;
    }
    println!("Bad Endpoint ---");
    {
        let client_blocked: bool = false;
        let client_requested_endpoint: &str = "endpoint";
        let client_requested_endpoint_valid: bool = false;
        let server_challenge: bson::document::Document = doc!("msg": "Speak friend and enter");
        let client_response: bson::document::Document = doc!("msg": "Mellon");
        let server_expected_response: bson::document::Document = doc!("msg": "Mellon");
        let should_fail: bool = true;
        identity_test(
            client_blocked,
            client_requested_endpoint,
            client_requested_endpoint_valid,
            server_challenge,
            client_response,
            server_expected_response,
            should_fail)?;
    }
    println!("Bad Challenge Response ---");
    {
        let client_blocked: bool = false;
        let client_requested_endpoint: &str = "endpoint";
        let client_requested_endpoint_valid: bool = true;
        let server_challenge: bson::document::Document = doc!("msg": "Speak friend and enter");
        let client_response: bson::document::Document = doc!("msg": "Friend?");
        let server_expected_response: bson::document::Document = doc!("msg": "Mellon");
        let should_fail: bool = true;
        identity_test(
            client_blocked,
            client_requested_endpoint,
            client_requested_endpoint_valid,
            server_challenge,
            client_response,
            server_expected_response,
            should_fail)?;
    }
    println!("Blocked Client ---");
    {
        let client_blocked: bool = true;
        let client_requested_endpoint: &str = "endpoint";
        let client_requested_endpoint_valid: bool = true;
        let server_challenge: bson::document::Document = doc!("msg": "Speak friend and enter");
        let client_response: bson::document::Document = doc!("msg": "Mellon");
        let server_expected_response: bson::document::Document = doc!("msg": "Mellon");
        let should_fail: bool = true;
        identity_test(
            client_blocked,
            client_requested_endpoint,
            client_requested_endpoint_valid,
            server_challenge,
            client_response,
            server_expected_response,
            should_fail)?;
    }
    Ok(())
}

#[cfg(test)]
fn endpoint_test(should_fail: bool, client_allowed: bool) -> Result<()> {

    // test sockets
    let stream1 = MemoryStream::new();
    let stream2 = MemoryStream::new();

    // server+client setup
    let server_ed25519_private = Ed25519PrivateKey::generate();
    let server_ed25519_public = Ed25519PublicKey::from_private_key(&server_ed25519_private);
    let server_service_id = V3OnionServiceId::from_public_key(&server_ed25519_public);

    let client_ed25519_private = Ed25519PrivateKey::generate();
    let client_ed25519_public = Ed25519PublicKey::from_private_key(&client_ed25519_private);
    let client_service_id = V3OnionServiceId::from_public_key(&client_ed25519_public);

    // ensure our client is in the allow list
    let allowed_client = if client_allowed {
        client_service_id.clone()
    } else {
        let ed25519_private = Ed25519PrivateKey::generate();
        let ed25519_public = Ed25519PublicKey::from_private_key(&ed25519_private);
        V3OnionServiceId::from_public_key(&ed25519_public)
    };

    let server_rpc = Session::new(stream1.clone(), stream2.clone());

    let mut endpoint_server = EndpointServer::new(server_rpc, allowed_client, server_service_id.clone());

    let client_rpc = Session::new(stream2, stream1);

    let mut endpoint_client = EndpointClient::new(client_rpc, server_service_id.clone(), "channel".to_string(), client_ed25519_private);

    let mut failure_ocurred = false;
    let mut server_complete = false;
    let mut client_complete = false;
    while !server_complete && !client_complete {
        if !server_complete {
            match endpoint_server.update() {
                Ok(Some(EndpointServerEvent::ChannelRequestReceived{
                    requested_channel})) => {
                    ensure!(requested_channel == "channel");
                },
                Ok(Some(EndpointServerEvent::HandshakeCompleted{
                    client_service_id: ret_client_service_id,
                    channel_name: ret_channel})) => {
                    ensure!(ret_client_service_id == client_service_id);
                    server_complete = true;
                },
                Ok(Some(EndpointServerEvent::HandshakeRejected{
                    client_allowed,
                    client_requested_channel_valid,
                    client_proof_signature_valid})) => {
                    println!("handshake rejected: client_allowed: {}, client_requested_channel_valid: {}, client_proof_signature_valid: {}", client_allowed, client_requested_channel_valid, client_proof_signature_valid);
                    server_complete = true;
                    failure_ocurred = true;
                },
                Ok(None) => {},
                Err(err) => {
                    println!("server failure: {:?}", err);
                    server_complete = true;
                    failure_ocurred = true;
                },
            }
        }

        if !client_complete {
            match endpoint_client.update() {
                Ok(Some(EndpointClientEvent::HandshakeCompleted)) => {
                    client_complete = true;
                },
                Ok(None) => {},
                Err(err) => {
                    println!("client failure: {:?}", err);
                    client_complete = true;
                    failure_ocurred = true;
                },
            }
        }
    }

    println!("server_complete: {}", server_complete);
    println!("client_complete: {}", client_complete);

    ensure!(should_fail == failure_ocurred);

    Ok(())
}

#[test]
fn test_endpoint_handshake() -> Result<()> {
    endpoint_test(false, true)?;
    endpoint_test(true, false)?;

    Ok(())
}

// Client Handshake

#[test]
#[serial]
fn test_gosling_context() -> Result<()> {

    let alice_private_key = Ed25519PrivateKey::generate();
    let alice_service_id = V3OnionServiceId::from_private_key(&alice_private_key);
    let mut alice_path = std::env::temp_dir();
    alice_path.push("test_gosling_context_alice");

    println!("Starting Alice gosling context ({})", alice_service_id.to_string());
    let mut alice = Context::new(
        &alice_path,
        420,
        420,
        alice_private_key)?;
    alice.bootstrap()?;

    let mut bootstrap_complete = false;
    while !bootstrap_complete {
        for event in alice.update()?.drain(..) {
            match event {
                ContextEvent::TorBootstrapStatusReceived{progress,tag,summary} => println!("Alice BootstrapStatus: {{ progress: {}, tag: {}, summary: '{}' }}", progress, tag, summary),
                ContextEvent::TorBootstrapCompleted => {
                    println!("Alice Bootstrap Complete!");
                    bootstrap_complete = true;
                },
                ContextEvent::TorLogReceived{line} => {
                    println!("--- ALICE --- {}", line);
                },
                _ => {},
            }
        }
    }

    let pat_private_key = Ed25519PrivateKey::generate();
    let pat_service_id = V3OnionServiceId::from_private_key(&pat_private_key);
    let mut pat_path = std::env::temp_dir();
    pat_path.push("test_gosling_context_pat");

    println!("Starting Pat gosling context ({})", pat_service_id.to_string());
    let mut pat = Context::new(
        &pat_path,
        420,
        420,
        pat_private_key)?;
    pat.bootstrap()?;

    let mut bootstrap_complete = false;
    while !bootstrap_complete {
        for event in pat.update()?.drain(..) {
            match event {
                ContextEvent::TorBootstrapStatusReceived{progress,tag,summary} => println!("Pat BootstrapStatus: {{ progress: {}, tag: {}, summary: '{}' }}", progress, tag, summary),
                ContextEvent::TorBootstrapCompleted => {
                    println!("Pat Bootstrap Complete!");
                    bootstrap_complete = true;
                },
                ContextEvent::TorLogReceived{line} => {
                    println!("--- PAT --- {}", line);
                },
                _ => {},
            }
        }
    }

    println!("Starting Alice identity server");
    alice.identity_server_start()?;

    println!("------------ Begin event loop ------------ ");

    let mut identity_published = false;
    let mut endpoint_published = false;
    let mut saved_endpoint_service_id: Option<V3OnionServiceId> = None;
    let mut saved_endpoint_client_auth_key: Option<X25519PrivateKey> = None;

    let mut alice_server_socket: Option<TcpStream> = None;
    let mut pat_client_socket: Option<TcpStream> = None;
    let mut pat_handshake_handle: usize = !0usize;

    while alice_server_socket.is_none() || pat_client_socket.is_none() {

        // update alice
        let mut events = alice.update()?;
        for event in events.drain(..) {
            match event {
                ContextEvent::IdentityServerPublished => {
                    if !identity_published {
                        println!("Alice: identity server published");

                        // alice has published the identity server, so pat may now request an endpoint
                        if let Ok(handle) = pat.identity_client_begin_handshake(alice_service_id.clone(), "test_endpoint") {
                            identity_published = true;
                            pat_handshake_handle = handle;
                        }
                    }
                },
                ContextEvent::EndpointServerPublished{endpoint_service_id, endpoint_name} => {
                    if !endpoint_published {
                        println!("Alice: endpoint server published");
                        println!(" endpoint_service_id: {}", endpoint_service_id.to_string());
                        println!(" endpoint_name: {}", endpoint_name);

                        // alice has published this endpoint, so pat may now connect

                        if let Some(saved_endpoint_service_id) = saved_endpoint_service_id.as_ref() {
                            ensure!(*saved_endpoint_service_id == endpoint_service_id);
                        }

                        if let Ok(()) = pat.endpoint_client_begin_handshake(saved_endpoint_service_id.clone().unwrap(),
                                                                            saved_endpoint_client_auth_key.clone().unwrap(),
                                                                            "test_channel".to_string()) {
                            endpoint_published = true;
                        }
                    }
                },
                ContextEvent::IdentityServerEndpointRequestReceived{handle, client_service_id, requested_endpoint} => {
                    println!("Alice: endpoint request received");
                    println!(" handle: {}", handle);
                    println!(" client_service_id: {}", client_service_id.to_string());
                    println!(" requested_endpoint: {}", requested_endpoint);
                    // auto accept endpoint request, send empty challenge
                    alice.identity_server_handle_endpoint_request_received(handle, true, true, doc!{})?;
                },
                ContextEvent::IdentityServerChallengeResponseReceived{handle, challenge_response} => {
                    println!("Alice: challenge response received");
                    println!(" handle: {}", handle);
                    println!(" challenge_response: {}", challenge_response);
                    // auto accept challenge response
                    alice.identity_server_handle_challenge_response_received(handle, true)?;
                },
                ContextEvent::IdentityServerHandshakeCompleted{handle, endpoint_private_key, endpoint_name, client_service_id, client_auth_public_key} => {
                    println!("Alice: endpoint request handled");
                    println!(" handle: {}", handle);
                    println!(" endpoint_service_id: {}", V3OnionServiceId::from_private_key(&endpoint_private_key).to_string());
                    println!(" endpoint: {}", endpoint_name);
                    println!(" client: {}", client_service_id.to_string());

                    // server handed out endpoint server info, so start the endpoint server
                    alice.endpoint_server_start(endpoint_private_key, endpoint_name, client_service_id, client_auth_public_key)?;
                },
                ContextEvent::EndpointServerHandshakeCompleted{handle, endpoint_service_id, client_service_id, channel_name, stream} => {
                    println!("Alice: endpoint channel accepted");
                    println!(" endpoint_service_id: {}", endpoint_service_id.to_string());
                    println!(" client_service_id: {}", client_service_id.to_string());
                    println!(" channel_name: {}", channel_name);
                    alice_server_socket = Some(stream);
                },
                ContextEvent::TorLogReceived{line} => {
                    println!("--- ALICE --- {}", line);
                },
                _ => bail!("Alice received unexpected event"),
            }
        }

        // update pat
        let mut events = pat.update()?;
        for event in events.drain(..) {
            match event {
                ContextEvent::IdentityClientChallengeReceived{handle, identity_service_id, endpoint_name, endpoint_challenge} => {
                    ensure!(handle == pat_handshake_handle);
                    println!("Pat: challenge request received");
                    println!(" handle: {}", handle);
                    println!(" identity_service_id: {}", identity_service_id.to_string());
                    println!(" endpoint_name: {}", endpoint_name);
                    println!(" endpoint_challenge: {}", endpoint_challenge);
                    pat.identity_client_handle_challenge_received(handle, doc!())?;
                },
                ContextEvent::IdentityClientHandshakeCompleted{handle, identity_service_id, endpoint_service_id, endpoint_name, client_auth_private_key} => {
                    ensure!(handle == pat_handshake_handle);
                    println!("Pat: endpoint request succeeded");
                    println!(" handle: {}", handle);
                    println!(" identity_service_id: {}", identity_service_id.to_string());
                    println!(" endpoint_service_id: {}", endpoint_service_id.to_string());
                    println!(" endpoint_name: {}", endpoint_name);
                    saved_endpoint_service_id = Some(endpoint_service_id);
                    saved_endpoint_client_auth_key = Some(client_auth_private_key);
                },
                ContextEvent::IdentityClientHandshakeFailed{handle,reason: Some(reason)} => {
                    println!("Pat: identity handshake aborted {:?}", reason);
                    println!(" handle: {}", handle);
                    println!(" reason: {:?}", reason);
                    bail!(reason);
                },
                ContextEvent::IdentityClientHandshakeFailed{handle,reason: None} => {
                    println!("Pat: identity handshake aborted");
                    println!(" handle: {}", handle);
                    println!(" reason: None");
                    bail!("no reason given");
                },
                ContextEvent::EndpointClientHandshakeCompleted{endpoint_service_id, channel_name, stream} => {
                    println!("Pat: endpoint channel opened");
                    println!(" endpoint_service_id: {}", endpoint_service_id.to_string());
                    println!(" channel_name: {}", channel_name);
                    pat_client_socket = Some(stream);
                },
                ContextEvent::TorLogReceived{line} => {
                    println!("--- PAT --- {}", line);
                },
                _ => bail!("Pat received unexpected event"),
            }
        }
    }

    let alice_server_socket = alice_server_socket.take().unwrap();
    let mut pat_client_socket = pat_client_socket.take().unwrap();

    resolve!(pat_client_socket.write(b"Hello World!\n"));
    resolve!(pat_client_socket.flush());

    resolve!(alice_server_socket.set_nonblocking(false));
    let mut alice_reader = BufReader::new(alice_server_socket);

    let mut response: String = Default::default();
    resolve!(alice_reader.read_line(&mut response));

    println!("response: '{}'", response);
    ensure!(response == "Hello World!\n");

    Ok(())
}